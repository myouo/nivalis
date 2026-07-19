use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::content::{ContentStaging, FileKey, RemoveOutcome};

use super::domain::{DbFailure, MessageId};

const MAX_FILES_PER_RUN: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct FileGcOutcome {
    pub(super) examined: u8,
    pub(super) referenced: u8,
    pub(super) removed: u8,
    pub(super) missing: u8,
    pub(super) invalid_keys: u8,
    pub(super) io_errors: u8,
}

pub(super) fn run_file_gc(
    connection: &mut Connection,
    staging: &ContentStaging,
    limit: usize,
) -> Result<FileGcOutcome, DbFailure> {
    if !(1..=MAX_FILES_PER_RUN).contains(&limit) {
        return Err(DbFailure::resource_limit(
            "file GC batch limit must be between 1 and 16",
        ));
    }

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    let candidates = load_candidates(&transaction, limit)?;
    let mut outcome = FileGcOutcome::default();

    for stored_key in candidates {
        outcome.examined += 1;
        let key = match FileKey::parse(&stored_key) {
            Ok(key) => key,
            Err(_) => {
                outcome.invalid_keys += 1;
                continue;
            }
        };

        // The immediate transaction prevents a writer from adding a reference
        // between this final check and the physical unlink.
        if is_referenced(&transaction, key.as_str())? {
            dequeue(&transaction, key.as_str())?;
            outcome.referenced += 1;
            continue;
        }

        match staging.remove_published_file(&key) {
            Ok(RemoveOutcome::Removed) => {
                dequeue(&transaction, key.as_str())?;
                outcome.removed += 1;
            }
            Ok(RemoveOutcome::Missing) => {
                dequeue(&transaction, key.as_str())?;
                outcome.missing += 1;
            }
            Err(_) => {
                outcome.io_errors += 1;
            }
        }
    }

    transaction.commit().map_err(DbFailure::database)?;
    Ok(outcome)
}

pub(super) fn attachment_file_key(
    connection: &Connection,
    message_id: MessageId,
    ordinal: u16,
) -> Result<Option<FileKey>, DbFailure> {
    let stored_key = connection
        .query_row(
            "SELECT file_key
             FROM attachments
             WHERE message_id = ?1 AND ordinal = ?2 AND file_key IS NOT NULL",
            params![message_id.get(), ordinal],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(DbFailure::database)?;

    stored_key
        .map(|stored_key| {
            if !stored_key.starts_with("attachment/") {
                return Err(DbFailure::database(
                    "non-attachment file key stored for attachment",
                ));
            }
            FileKey::parse(&stored_key)
                .map_err(|_| DbFailure::database("invalid attachment file key in database"))
        })
        .transpose()
}

fn load_candidates(transaction: &Transaction<'_>, limit: usize) -> Result<Vec<String>, DbFailure> {
    let mut statement = transaction
        .prepare(
            "SELECT file_key
             FROM file_gc
             ORDER BY queued_at_ms, file_key
             LIMIT ?1",
        )
        .map_err(DbFailure::database)?;
    let rows = statement
        .query_map([limit as i64], |row| row.get::<_, String>(0))
        .map_err(DbFailure::database)?;
    let mut candidates = Vec::with_capacity(limit);
    for row in rows {
        candidates.push(row.map_err(DbFailure::database)?);
    }
    Ok(candidates)
}

fn is_referenced(transaction: &Transaction<'_>, file_key: &str) -> Result<bool, DbFailure> {
    let referenced = transaction
        .query_row(
            "SELECT
                 EXISTS (
                     SELECT 1 FROM message_content WHERE body_file_key = ?1
                 ) OR EXISTS (
                     SELECT 1 FROM attachments WHERE file_key = ?1
                 ) OR EXISTS (
                     SELECT 1 FROM outbox WHERE mime_file_key = ?1
                 ) OR EXISTS (
                     SELECT 1 FROM file_staging WHERE file_key = ?1
                 )",
            [file_key],
            |row| row.get::<_, i64>(0),
        )
        .map_err(DbFailure::database)?;
    Ok(referenced != 0)
}

fn dequeue(transaction: &Transaction<'_>, file_key: &str) -> Result<(), DbFailure> {
    transaction
        .execute("DELETE FROM file_gc WHERE file_key = ?1", [file_key])
        .map_err(DbFailure::database)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use rusqlite::{Connection, params};

    use crate::{
        content::{ContentStaging, FileKey},
        store::sqlite::{domain::FailureKind, migrations::migrate},
    };

    use super::{FileGcOutcome, attachment_file_key, run_file_gc};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock follows Unix epoch")
                .as_nanos();
            Self(std::env::temp_dir().join(format!(
                "nivalis-file-gc-{label}-{}-{timestamp}-{sequence}",
                std::process::id()
            )))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn database() -> Connection {
        let mut connection = Connection::open_in_memory().expect("open in-memory database");
        migrate(&mut connection).expect("migrate database");
        connection
    }

    fn staging(label: &str) -> (TestRoot, ContentStaging) {
        let root = TestRoot::new(label);
        let staging = ContentStaging::open(root.path().to_path_buf()).expect("open content root");
        (root, staging)
    }

    fn file_key(kind: &str, value: u128) -> FileKey {
        let extension = match kind {
            "body" => "txt",
            "attachment" => "bin",
            _ => panic!("unsupported test file kind"),
        };
        FileKey::parse(&format!("{kind}/{value:032x}.{extension}"))
            .expect("construct valid file key")
    }

    fn published_path(root: &TestRoot, key: &FileKey) -> PathBuf {
        root.path().join(key.as_str())
    }

    fn write_published(root: &TestRoot, key: &FileKey) {
        fs::write(published_path(root, key), b"published content")
            .expect("write published content");
    }

    fn queue(connection: &Connection, key: &str, queued_at_ms: i64) {
        connection
            .execute(
                "INSERT INTO file_gc (file_key, queued_at_ms) VALUES (?1, ?2)",
                params![key, queued_at_ms],
            )
            .expect("queue file for GC");
    }

    fn queue_count(connection: &Connection) -> i64 {
        connection
            .query_row("SELECT count(*) FROM file_gc", [], |row| row.get(0))
            .expect("count queued files")
    }

    fn insert_account(connection: &Connection) {
        connection
            .execute(
                "INSERT INTO accounts
                 (id, provider, remote_key, name, address, sort_order, state, accent_rgb)
                 VALUES (1, 'imap', 'account', 'Account', 'mail@example.test', 0, 'active', 0)",
                [],
            )
            .expect("insert account");
    }

    fn insert_message(connection: &Connection, id: i64) {
        connection
            .execute(
                "INSERT INTO messages (id, account_id, remote_key, received_at_ms)
                 VALUES (?1, 1, ?2, 0)",
                params![id, format!("message-{id}")],
            )
            .expect("insert message");
    }

    #[test]
    fn rechecks_all_reference_kinds_before_unlinking() {
        let mut connection = database();
        insert_account(&connection);
        for id in 1..=4 {
            insert_message(&connection, id);
        }
        let (root, staging) = staging("references");
        let body = file_key("body", 1);
        let attachment = file_key("attachment", 2);
        let outbox = file_key("body", 3);
        let staged = file_key("attachment", 4);
        for key in [&body, &attachment, &outbox, &staged] {
            write_published(&root, key);
        }

        connection
            .execute(
                "INSERT INTO message_content (message_id, body_file_key) VALUES (1, ?1)",
                [body.as_str()],
            )
            .expect("reference body file");
        connection
            .execute(
                "INSERT INTO attachments (id, message_id, ordinal, file_key)
                 VALUES (1, 2, 0, ?1)",
                [attachment.as_str()],
            )
            .expect("reference attachment file");
        connection
            .execute(
                "INSERT INTO outbox
                 (message_id, mime_file_key, envelope_from, wire_byte_count, state)
                 VALUES (3, ?1, 'sender@example.test', 1, 'ready')",
                [outbox.as_str()],
            )
            .expect("reference outbox file");
        connection
            .execute(
                "INSERT INTO file_staging
                 (batch_token, file_key, message_id, account_id, content_generation,
                  file_kind, part_ordinal, created_at_ms, expires_at_ms)
                 VALUES ('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', ?1, 4, 1, 1,
                         'attachment', 0, 0, 1)",
                [staged.as_str()],
            )
            .expect("reference staged file");

        // Recreate stale queue entries after the references to model a race or
        // recovery from an older writer that did not run the dequeue triggers.
        for (queued_at_ms, key) in [&body, &attachment, &outbox, &staged]
            .into_iter()
            .enumerate()
        {
            queue(&connection, key.as_str(), queued_at_ms as i64);
        }

        let outcome = run_file_gc(&mut connection, &staging, 4).expect("run file GC");

        assert_eq!(
            outcome,
            FileGcOutcome {
                examined: 4,
                referenced: 4,
                ..FileGcOutcome::default()
            }
        );
        assert_eq!(queue_count(&connection), 0);
        for key in [&body, &attachment, &outbox, &staged] {
            assert!(published_path(&root, key).is_file());
        }
    }

    #[test]
    fn removes_unreferenced_files_and_converges_when_already_missing() {
        let mut connection = database();
        let (root, staging) = staging("removed-missing");
        let present = file_key("body", 10);
        let missing = file_key("attachment", 11);
        write_published(&root, &present);
        queue(&connection, present.as_str(), 1);
        queue(&connection, missing.as_str(), 2);

        let outcome = run_file_gc(&mut connection, &staging, 2).expect("run file GC");

        assert_eq!(
            outcome,
            FileGcOutcome {
                examined: 2,
                removed: 1,
                missing: 1,
                ..FileGcOutcome::default()
            }
        );
        assert!(!published_path(&root, &present).exists());
        assert_eq!(queue_count(&connection), 0);
    }

    #[cfg(unix)]
    #[test]
    fn unlinking_managed_symlink_does_not_touch_external_target() {
        use std::os::unix::fs::symlink;

        let mut connection = database();
        let (root, staging) = staging("symlink");
        let external = TestRoot::new("symlink-external");
        fs::create_dir_all(external.path()).expect("create external directory");
        let external_file = external.path().join("outside.txt");
        fs::write(&external_file, b"outside").expect("write external file");
        let key = file_key("body", 20);
        let managed_path = published_path(&root, &key);
        symlink(&external_file, &managed_path).expect("create managed symlink");
        queue(&connection, key.as_str(), 1);

        let outcome = run_file_gc(&mut connection, &staging, 1).expect("run file GC");

        assert_eq!(outcome.removed, 1);
        assert!(!managed_path.exists());
        assert_eq!(
            fs::read(&external_file).expect("read external file"),
            b"outside"
        );
        assert_eq!(queue_count(&connection), 0);
    }

    #[test]
    fn applies_exact_batch_limit_and_rejects_out_of_range_limits() {
        let mut connection = database();
        let (_root, staging) = staging("limit");
        let keys = [
            file_key("body", 30),
            file_key("body", 31),
            file_key("body", 32),
        ];
        for (index, key) in keys.iter().enumerate() {
            queue(&connection, key.as_str(), index as i64);
        }

        let first = run_file_gc(&mut connection, &staging, 2).expect("run bounded file GC");
        assert_eq!(first.examined, 2);
        assert_eq!(first.missing, 2);
        assert_eq!(queue_count(&connection), 1);
        let remaining: String = connection
            .query_row("SELECT file_key FROM file_gc", [], |row| row.get(0))
            .expect("read remaining key");
        assert_eq!(remaining, keys[2].as_str());

        let zero = run_file_gc(&mut connection, &staging, 0).expect_err("reject zero limit");
        let excessive =
            run_file_gc(&mut connection, &staging, 17).expect_err("reject excessive limit");
        assert_eq!(zero.kind, FailureKind::ResourceLimit);
        assert_eq!(excessive.kind, FailureKind::ResourceLimit);
        assert_eq!(queue_count(&connection), 1);

        let second = run_file_gc(&mut connection, &staging, 1).expect("drain remaining file");
        assert_eq!(second.examined, 1);
        assert_eq!(second.missing, 1);
        assert_eq!(queue_count(&connection), 0);
    }

    #[test]
    fn invalid_keys_and_io_errors_remain_without_blocking_later_candidates() {
        let mut connection = database();
        let (root, staging) = staging("failures");
        let invalid = "body/not-a-valid-key.txt";
        let io_error = file_key("attachment", 40);
        let missing = file_key("body", 41);
        fs::create_dir(published_path(&root, &io_error)).expect("create non-file at managed path");
        queue(&connection, invalid, 1);
        queue(&connection, io_error.as_str(), 2);
        queue(&connection, missing.as_str(), 3);

        let outcome = run_file_gc(&mut connection, &staging, 3).expect("run file GC");

        assert_eq!(
            outcome,
            FileGcOutcome {
                examined: 3,
                missing: 1,
                invalid_keys: 1,
                io_errors: 1,
                ..FileGcOutcome::default()
            }
        );
        let mut statement = connection
            .prepare("SELECT file_key FROM file_gc ORDER BY queued_at_ms")
            .expect("prepare retained queue query");
        let retained = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query retained keys")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect retained keys");
        assert_eq!(retained, [invalid, io_error.as_str()]);
    }

    #[test]
    fn attachment_key_lookup_validates_message_ownership_and_key_kind() {
        let connection = database();
        insert_account(&connection);
        insert_message(&connection, 1);
        insert_message(&connection, 2);
        let valid = file_key("attachment", 50);
        let wrong_kind = file_key("body", 51);
        connection
            .execute(
                "INSERT INTO attachments (id, message_id, ordinal, file_key)
                 VALUES (1, 1, 0, ?1),
                        (2, 1, 1, ?2),
                        (3, 1, 2, NULL),
                        (4, 1, 3, 'attachment/not-valid.bin')",
                params![valid.as_str(), wrong_kind.as_str()],
            )
            .expect("insert attachment records");

        let message_one =
            crate::store::sqlite::domain::MessageId::new(1).expect("construct message id");
        let message_two =
            crate::store::sqlite::domain::MessageId::new(2).expect("construct message id");
        assert_eq!(
            attachment_file_key(&connection, message_one, 0)
                .expect("load valid attachment key")
                .as_ref()
                .map(FileKey::as_str),
            Some(valid.as_str())
        );
        assert_eq!(
            attachment_file_key(&connection, message_two, 0).expect("verify message ownership"),
            None
        );
        assert_eq!(
            attachment_file_key(&connection, message_one, 2).expect("load null key"),
            None
        );
        assert_eq!(
            attachment_file_key(&connection, message_one, 1)
                .expect_err("reject body key for attachment")
                .kind,
            FailureKind::Database
        );
        assert_eq!(
            attachment_file_key(&connection, message_one, 3)
                .expect_err("reject malformed attachment key")
                .kind,
            FailureKind::Database
        );
    }
}
