use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::domain::DbFailure;

const BACKFILL_BATCH: usize = 16;

struct SearchDocument {
    id: i64,
    sender_name: String,
    sender_address: String,
    subject: String,
    preview: String,
    body: String,
}

pub(super) fn backfill_complete(connection: &Connection) -> Result<bool, DbFailure> {
    connection
        .query_row(
            "SELECT complete FROM message_search_backfill WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
}

pub(super) fn backfill_next_batch(connection: &mut Connection) -> Result<bool, DbFailure> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    let (next_message_id, already_complete) = transaction
        .query_row(
            "SELECT next_message_id, complete
               FROM message_search_backfill
              WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
        )
        .optional()
        .map_err(DbFailure::database)?
        .ok_or_else(|| DbFailure::conflict("message search backfill state is missing"))?;
    if already_complete {
        return Ok(true);
    }
    let documents = {
        let mut statement = transaction
            .prepare(
                "SELECT message.id,
                        message.sender_name,
                        message.sender_address,
                        message.subject,
                        message.preview,
                        coalesce(content.reader_excerpt, '')
                   FROM messages AS message
                   LEFT JOIN message_content AS content ON content.message_id = message.id
                  WHERE message.id >= ?1
                  ORDER BY message.id
                  LIMIT ?2",
            )
            .map_err(DbFailure::database)?;
        statement
            .query_map(
                params![
                    next_message_id,
                    i64::try_from(BACKFILL_BATCH).expect("batch fits i64")
                ],
                |row| {
                    Ok(SearchDocument {
                        id: row.get(0)?,
                        sender_name: row.get(1)?,
                        sender_address: row.get(2)?,
                        subject: row.get(3)?,
                        preview: row.get(4)?,
                        body: row.get(5)?,
                    })
                },
            )
            .map_err(DbFailure::database)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(DbFailure::database)?
    };
    for document in &documents {
        transaction
            .execute(
                "INSERT OR IGNORE INTO message_search_documents
                     (rowid, sender_name, sender_address, subject, preview, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    document.id,
                    document.sender_name,
                    document.sender_address,
                    document.subject,
                    document.preview,
                    document.body,
                ],
            )
            .map_err(DbFailure::database)?;
    }
    let complete = documents.len() < BACKFILL_BATCH
        || documents
            .last()
            .is_some_and(|document| document.id == i64::MAX);
    let next_message_id = documents
        .last()
        .and_then(|document| document.id.checked_add(1))
        .unwrap_or(next_message_id);
    transaction
        .execute(
            "UPDATE message_search_backfill
                SET next_message_id = ?1, complete = ?2
              WHERE singleton = 1",
            params![next_message_id, complete],
        )
        .map_err(DbFailure::database)?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(complete)
}
