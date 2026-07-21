use std::{
    hint::black_box,
    path::{Path, PathBuf},
    time::Duration,
};

use criterion::{Criterion, criterion_group, criterion_main};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};

const MIGRATIONS: &[&str] = &[
    include_str!("../../migrations/0001_init.sql"),
    include_str!("../../migrations/0002_mail_mutations.sql"),
    include_str!("../../migrations/0003_file_reference_indexes.sql"),
    include_str!("../../migrations/0004_mutation_guards.sql"),
    include_str!("../../migrations/0005_clean_stale_file_gc.sql"),
    include_str!("../../migrations/0006_account_mailbox_stats.sql"),
    include_str!("../../migrations/0007_remote_change_journal.sql"),
    include_str!("../../migrations/0008_remote_lease_reservations.sql"),
    include_str!("../../migrations/0009_message_search.sql"),
    include_str!("../../migrations/0010_content_file_lifecycle.sql"),
    include_str!("../../migrations/0011_account_configuration.sql"),
    include_str!("../../migrations/0012_drafts_outbox.sql"),
    include_str!("../../migrations/0013_repair_draft_stats.sql"),
    include_str!("../../migrations/0014_inbox_history_backfill.sql"),
    include_str!("../../migrations/0015_full_text_body_search.sql"),
];

const SEARCH_CASES: &[(&str, &str)] = &[
    ("sender", "{sender_name sender_address} : \"needle\""),
    ("subject", "subject : \"needle\""),
    ("body", "body : \"needle\""),
    (
        "combined",
        "{sender_name sender_address} : \"needle\" AND subject : \"needle\" AND body : \"needle\"",
    ),
    ("chinese", "\"项目计划\""),
];

struct Fixture {
    path: PathBuf,
    reader: Connection,
    cached_message_id: i64,
}

impl Fixture {
    fn new(message_count: usize) -> Self {
        let path = std::env::temp_dir().join(format!(
            "nivalis-local-store-{}-{message_count}.sqlite3",
            std::process::id()
        ));
        remove_database_files(&path);
        let mut writer = Connection::open(&path).expect("open benchmark database");
        writer
            .busy_timeout(Duration::from_secs(5))
            .expect("configure busy timeout");
        writer
            .pragma_update(None, "journal_mode", "WAL")
            .expect("enable WAL");
        writer
            .pragma_update(None, "synchronous", "OFF")
            .expect("speed benchmark fixture construction");
        for migration in MIGRATIONS {
            writer.execute_batch(migration).expect("apply migration");
        }
        seed_messages(&mut writer, message_count);
        writer
            .execute_batch(
                "INSERT INTO message_search(message_search, rank) VALUES ('optimize', 0);
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .expect("optimize benchmark index");
        drop(writer);

        let reader = open_reader(&path);
        warm_reader(&reader);
        Self {
            path,
            reader,
            cached_message_id: i64::try_from(message_count).expect("fixture count fits i64"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        remove_database_files(&self.path);
    }
}

fn open_reader(path: &Path) -> Connection {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let reader = Connection::open_with_flags(path, flags).expect("open benchmark reader");
    reader
        .busy_timeout(Duration::from_secs(1))
        .expect("configure reader timeout");
    reader
        .pragma_update(None, "query_only", true)
        .expect("make reader query-only");
    reader
        .pragma_update(None, "cache_size", -512_i64)
        .expect("bound reader cache");
    reader
        .pragma_update(None, "mmap_size", 0_i64)
        .expect("disable mmap");
    reader
        .pragma_update(None, "temp_store", "FILE")
        .expect("bound temporary sorting outside the heap");
    reader
}

fn seed_messages(connection: &mut Connection, count: usize) {
    connection
        .execute_batch(
            "INSERT INTO accounts
                 (id, provider, remote_key, name, address, state, accent_rgb)
             VALUES (1, 'imap', 'benchmark', 'Benchmark', 'bench@example.test', 'active', 0);
             INSERT INTO folders (id, account_id, remote_key, name, role)
             VALUES (1, 1, 'INBOX', 'Inbox', 'inbox');",
        )
        .expect("seed benchmark account");
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin benchmark seed");
    {
        let mut insert_message = transaction
            .prepare(
                "INSERT INTO messages
                     (id, account_id, remote_key, sender_name, sender_address,
                      subject, preview, received_at_ms, unread, starred)
                 VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
            )
            .expect("prepare message insert");
        let mut insert_folder = transaction
            .prepare(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (?1, 1, 1)",
            )
            .expect("prepare folder insert");
        let mut insert_content = transaction
            .prepare(
                "INSERT INTO message_content
                     (message_id, reader_excerpt, truncated, body_byte_count)
                 VALUES (?1, ?2, 0, ?3)",
            )
            .expect("prepare content insert");
        for id in 1..=count {
            let target = id % 1_000 == 0;
            let cached = id == count;
            let sender_name = if target {
                "Needle Sender".to_owned()
            } else {
                format!("Sender {id}")
            };
            let sender_address = if target {
                "needle@example.test".to_owned()
            } else {
                format!("sender-{id}@example.test")
            };
            let subject = if target {
                "Quarterly needle subject 项目计划".to_owned()
            } else {
                format!("Status report {id}")
            };
            let preview = if target {
                "Needle preview for indexed mail".to_owned()
            } else {
                format!("Ordinary preview {id}")
            };
            let body = if cached {
                "x".repeat(65_536)
            } else if target {
                "Cached needle body with 项目计划 and 合同审核".to_owned()
            } else {
                format!("Ordinary cached body for message {id}")
            };
            let id = i64::try_from(id).expect("fixture identity fits i64");
            insert_message
                .execute(params![
                    id,
                    format!("message-{id}"),
                    sender_name,
                    sender_address,
                    subject,
                    preview,
                    1_900_000_000_000_i64 - id,
                    id % 2,
                ])
                .expect("insert benchmark message");
            insert_folder
                .execute([id])
                .expect("insert benchmark folder membership");
            insert_content
                .execute(params![id, body, body.len() as i64])
                .expect("insert benchmark body");
        }
    }
    transaction.commit().expect("commit benchmark seed");
}

type MailRow = (
    i64,
    i64,
    String,
    String,
    String,
    String,
    i64,
    bool,
    bool,
    bool,
);

fn mail_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MailRow> {
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
    ))
}

fn first_screen(connection: &Connection, limit: i64) -> Vec<MailRow> {
    connection
        .prepare(
            "SELECT m.id, m.account_id, m.sender_name, m.sender_address,
                    m.subject, m.preview, m.received_at_ms,
                    m.unread, m.starred, m.has_attachment
               FROM messages AS m
              WHERE m.account_id = 1
                AND EXISTS (
                    SELECT 1
                      FROM message_folders AS mf
                      JOIN folders AS f
                        ON f.id = mf.folder_id AND f.account_id = mf.account_id
                     WHERE mf.message_id = m.id AND mf.account_id = m.account_id
                       AND f.role = 'inbox'
                )
                AND NOT EXISTS (
                    SELECT 1
                      FROM message_folders AS trash_mf
                      JOIN folders AS trash_f
                        ON trash_f.id = trash_mf.folder_id
                       AND trash_f.account_id = trash_mf.account_id
                     WHERE trash_mf.message_id = m.id
                       AND trash_mf.account_id = m.account_id
                       AND trash_f.role = 'trash'
                )
              ORDER BY m.received_at_ms DESC, m.id DESC
              LIMIT ?1",
        )
        .expect("prepare first-screen query")
        .query_map([limit], mail_row)
        .expect("query first screen")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect first screen")
}

fn search(connection: &Connection, expression: &str) -> Vec<MailRow> {
    connection
        .prepare(
            "SELECT m.id, m.account_id, m.sender_name, m.sender_address,
                    m.subject, m.preview, m.received_at_ms,
                    m.unread, m.starred, m.has_attachment
               FROM message_search
               JOIN messages AS m ON m.id = message_search.rowid
              WHERE message_search MATCH ?1
                AND m.account_id = 1
                AND EXISTS (
                    SELECT 1
                      FROM message_folders AS mf
                     WHERE mf.message_id = m.id
                       AND mf.folder_id = 1
                       AND mf.account_id = m.account_id
                )
              ORDER BY m.received_at_ms DESC, m.id DESC
              LIMIT 50",
        )
        .expect("prepare search query")
        .query_map([expression], mail_row)
        .expect("query search index")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect search results")
}

fn cached_body(connection: &Connection, message_id: i64) -> String {
    connection
        .query_row(
            "SELECT content.reader_excerpt
               FROM messages AS message
               JOIN message_content AS content ON content.message_id = message.id
              WHERE message.id = ?1",
            [message_id],
            |row| row.get(0),
        )
        .expect("read cached body")
}

fn warm_reader(connection: &Connection) {
    black_box(first_screen(connection, 50));
    for (_, expression) in SEARCH_CASES {
        black_box(search(connection, expression));
    }
}

fn local_store_benchmarks(criterion: &mut Criterion) {
    for count in [10_000, 100_000] {
        let fixture = Fixture::new(count);
        let mut group = criterion.benchmark_group(format!("nivalis/local_store/{count}"));
        group.sample_size(20);
        group.measurement_time(Duration::from_secs(3));
        group.bench_function("login_first_screen_20_local", |bencher| {
            bencher.iter(|| black_box(first_screen(&fixture.reader, black_box(20))))
        });
        group.bench_function("recent_50_operable_local", |bencher| {
            bencher.iter(|| black_box(first_screen(&fixture.reader, black_box(50))))
        });
        group.bench_function("cached_body_64k", |bencher| {
            bencher.iter(|| {
                black_box(cached_body(
                    &fixture.reader,
                    black_box(fixture.cached_message_id),
                ))
            })
        });
        for (name, expression) in SEARCH_CASES {
            group.bench_function(format!("search_{name}"), |bencher| {
                bencher.iter(|| black_box(search(&fixture.reader, black_box(expression))))
            });
        }
        group.bench_function("first_screen_during_background_write", |bencher| {
            let mut writer = Connection::open(&fixture.path).expect("open background writer");
            let transaction = writer
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("begin background write");
            transaction
                .execute("UPDATE messages SET unread = 1 - unread WHERE id = 1", [])
                .expect("hold background write");
            bencher.iter(|| black_box(first_screen(&fixture.reader, black_box(20))));
            transaction.rollback().expect("rollback background write");
        });
        group.finish();
    }
}

fn remove_database_files(path: &Path) {
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        let _ = std::fs::remove_file(candidate);
    }
}

criterion_group!(benches, local_store_benchmarks);
criterion_main!(benches);
