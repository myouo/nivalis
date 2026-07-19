use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::content::{ContentRecord, FileKey};

use super::domain::{DbFailure, MessageId};

const MIN_TIMESTAMP_MS: i64 = -62_135_596_800_000;
const MAX_TIMESTAMP_MS: i64 = 253_402_300_799_999;
const MAX_STAGING_TTL_MS: i64 = 15 * 60 * 1_000;
const MAX_STAGED_FILES: usize = 256;
const MAX_CONTENT_FILES: usize = 33;
const MAX_ATTACHMENTS: usize = 32;
const MAX_EXPIRED_BATCHES_PER_RESERVE: usize = 8;
const MAX_BODY_BYTES: u64 = 1_099_511_627_776;
const MAX_SUBJECT_BYTES: usize = 998;
const MAX_ADDRESS_BYTES: usize = 320;
const MAX_PREVIEW_BYTES: usize = 2_048;
const MAX_READER_EXCERPT_BYTES: usize = 65_536;
const MAX_FILE_NAME_BYTES: usize = 998;
const MAX_MEDIA_TYPE_BYTES: usize = 255;
const MAX_CONTENT_ID_BYTES: usize = 998;
const MAX_DISPOSITION_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ContentBatchToken([u8; 16]);

impl ContentBatchToken {
    pub(crate) fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    fn encoded(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(32);
        for byte in self.0 {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContentManifest {
    body_file_key: Option<FileKey>,
    attachments: Box<[(u16, FileKey)]>,
}

impl ContentManifest {
    pub(crate) fn new(
        body_file_key: Option<FileKey>,
        attachments: Vec<(u16, FileKey)>,
    ) -> Result<Self, DbFailure> {
        let manifest = Self {
            body_file_key,
            attachments: attachments.into_boxed_slice(),
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn from_record(record: &ContentRecord) -> Result<Self, DbFailure> {
        Self::new(
            record.body_file_key.clone(),
            record
                .attachments
                .iter()
                .map(|attachment| (attachment.ordinal, attachment.file_key.clone()))
                .collect(),
        )
    }

    pub(crate) fn file_count(&self) -> usize {
        usize::from(self.body_file_key.is_some()) + self.attachments.len()
    }

    pub(crate) fn body_file_key(&self) -> Option<&FileKey> {
        self.body_file_key.as_ref()
    }

    pub(crate) fn attachments(&self) -> &[(u16, FileKey)] {
        &self.attachments
    }

    fn validate(&self) -> Result<(), DbFailure> {
        let file_count = self.file_count();
        if self.body_file_key.is_none() {
            return Err(DbFailure::resource_limit(
                "content reservation must contain exactly one body file",
            ));
        }
        if file_count > MAX_CONTENT_FILES || self.attachments.len() > MAX_ATTACHMENTS {
            return Err(DbFailure::resource_limit(format!(
                "content reservation exceeds the {MAX_CONTENT_FILES}-file limit"
            )));
        }
        if let Some(key) = &self.body_file_key
            && !key.as_str().starts_with("body/")
        {
            return Err(DbFailure::conflict(
                "content body uses an attachment file key",
            ));
        }

        for (index, (ordinal, key)) in self.attachments.iter().enumerate() {
            if usize::from(*ordinal) != index {
                return Err(DbFailure::conflict(
                    "content attachment ordinals must be contiguous from zero",
                ));
            }
            if !key.as_str().starts_with("attachment/") {
                return Err(DbFailure::conflict(
                    "content attachment uses a body file key",
                ));
            }
            if self
                .body_file_key
                .as_ref()
                .is_some_and(|body_key| body_key == key)
                || self.attachments[..index]
                    .iter()
                    .any(|(_, earlier_key)| earlier_key == key)
            {
                return Err(DbFailure::conflict(
                    "content reservation contains duplicate file keys",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReserveContentRequest {
    message_id: MessageId,
    account_id: i64,
    batch_token: ContentBatchToken,
    manifest: ContentManifest,
    created_at_ms: i64,
    expires_at_ms: i64,
}

impl ReserveContentRequest {
    pub(crate) fn new(
        message_id: MessageId,
        account_id: i64,
        batch_token: ContentBatchToken,
        manifest: ContentManifest,
        created_at_ms: i64,
        expires_at_ms: i64,
    ) -> Result<Self, DbFailure> {
        validate_account_id(account_id)?;
        validate_lease(created_at_ms, expires_at_ms)?;
        manifest.validate()?;
        Ok(Self {
            message_id,
            account_id,
            batch_token,
            manifest,
            created_at_ms,
            expires_at_ms,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContentReservation {
    message_id: MessageId,
    account_id: i64,
    generation: i64,
    batch_token: ContentBatchToken,
    manifest: ContentManifest,
    created_at_ms: i64,
    expires_at_ms: i64,
}

impl ContentReservation {
    pub(crate) fn message_id(&self) -> MessageId {
        self.message_id
    }

    pub(crate) fn account_id(&self) -> i64 {
        self.account_id
    }

    pub(crate) fn generation(&self) -> i64 {
        self.generation
    }

    pub(crate) fn batch_token(&self) -> ContentBatchToken {
        self.batch_token
    }

    pub(crate) fn manifest(&self) -> &ContentManifest {
        &self.manifest
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContentFinalizeOutcome {
    pub(crate) generation: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentAbortOutcome {
    Released,
    AlreadyReleased,
}

pub(super) fn reserve_content(
    connection: &mut Connection,
    request: ReserveContentRequest,
) -> Result<ContentReservation, DbFailure> {
    validate_account_id(request.account_id)?;
    validate_lease(request.created_at_ms, request.expires_at_ms)?;
    request.manifest.validate()?;

    let transaction = immediate_transaction(connection)?;
    let Some((database_account_id, generation)) =
        load_message_identity(&transaction, request.message_id)?
    else {
        return Err(DbFailure::not_found("message no longer exists"));
    };
    if database_account_id != request.account_id {
        return Err(DbFailure::conflict(
            "content reservation account does not own the message",
        ));
    }
    let next_generation = generation
        .checked_add(1)
        .ok_or_else(|| DbFailure::resource_limit("content generation overflow"))?;

    let encoded_token = request.batch_token.encoded();
    let token_exists: bool = transaction
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM file_staging WHERE batch_token = ?1)",
            [&encoded_token],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    if token_exists {
        return Err(DbFailure::conflict(
            "content reservation batch token is already in use",
        ));
    }

    reap_expired_reservations(&transaction, request.created_at_ms)?;
    let staged_file_count: i64 = transaction
        .query_row(
            "SELECT file_count FROM file_staging_usage WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    let staged_file_count = usize::try_from(staged_file_count)
        .map_err(|_| DbFailure::resource_limit("invalid file staging usage"))?;
    let next_staged_file_count = staged_file_count
        .checked_add(request.manifest.file_count())
        .ok_or_else(|| DbFailure::resource_limit("file staging usage overflow"))?;
    if next_staged_file_count > MAX_STAGED_FILES {
        // Persist bounded recovery progress even when this reservation cannot fit yet.
        transaction.commit().map_err(DbFailure::database)?;
        return Err(DbFailure::resource_limit(format!(
            "file staging exceeds the {MAX_STAGED_FILES}-file limit"
        )));
    }

    let updated = transaction
        .execute(
            "UPDATE messages
             SET content_generation = ?3
             WHERE id = ?1 AND account_id = ?2 AND content_generation = ?4",
            params![
                request.message_id.get(),
                request.account_id,
                next_generation,
                generation
            ],
        )
        .map_err(DbFailure::database)?;
    if updated != 1 {
        return Err(DbFailure::conflict(
            "message content generation changed while reserving files",
        ));
    }

    if let Some(body_file_key) = request.manifest.body_file_key() {
        insert_staged_file(
            &transaction,
            &encoded_token,
            &request,
            next_generation,
            body_file_key,
            "body",
            None,
        )?;
    }
    for (ordinal, file_key) in request.manifest.attachments() {
        insert_staged_file(
            &transaction,
            &encoded_token,
            &request,
            next_generation,
            file_key,
            "attachment",
            Some(i64::from(*ordinal)),
        )?;
    }

    transaction.commit().map_err(DbFailure::database)?;
    Ok(ContentReservation {
        message_id: request.message_id,
        account_id: request.account_id,
        generation: next_generation,
        batch_token: request.batch_token,
        manifest: request.manifest,
        created_at_ms: request.created_at_ms,
        expires_at_ms: request.expires_at_ms,
    })
}

#[cfg(test)]
pub(super) fn finalize_content(
    connection: &mut Connection,
    reservation: &ContentReservation,
    record: &ContentRecord,
    now_ms: i64,
) -> Result<ContentFinalizeOutcome, DbFailure> {
    finalize_content_with_commit_hook(connection, reservation, record, now_ms, || {})
}

pub(super) fn finalize_content_with_commit_hook(
    connection: &mut Connection,
    reservation: &ContentReservation,
    record: &ContentRecord,
    now_ms: i64,
    before_commit: impl FnOnce(),
) -> Result<ContentFinalizeOutcome, DbFailure> {
    validate_timestamp(now_ms)?;
    validate_record(record)?;
    let record_manifest = ContentManifest::from_record(record)?;
    if record_manifest != reservation.manifest {
        return Err(DbFailure::conflict(
            "published content does not match its reservation",
        ));
    }
    if now_ms >= reservation.expires_at_ms {
        return Err(DbFailure::conflict("content reservation has expired"));
    }

    let transaction = immediate_transaction(connection)?;
    verify_current_generation(&transaction, reservation)?;
    verify_persisted_reservation(&transaction, reservation)?;
    let old_file_keys = load_existing_file_keys(&transaction, reservation.message_id)?;

    let body_byte_count = database_byte_count(record.body_byte_count, "content body")?;
    let updated = transaction
        .execute(
            "UPDATE messages
             SET sender_name = ?3,
                 sender_address = ?4,
                 subject = ?5,
                 preview = ?6,
                 received_at_ms = COALESCE(?7, received_at_ms),
                 has_attachment = ?8
             WHERE id = ?1 AND account_id = ?2 AND content_generation = ?9",
            params![
                reservation.message_id.get(),
                reservation.account_id,
                record.sender_name.as_ref(),
                record.sender_address.as_ref(),
                record.subject.as_ref(),
                record.preview.as_ref(),
                record.received_at_ms,
                !record.attachments.is_empty(),
                reservation.generation,
            ],
        )
        .map_err(DbFailure::database)?;
    if updated != 1 {
        return Err(DbFailure::conflict(
            "message changed while finalizing content",
        ));
    }

    transaction
        .execute(
            "INSERT INTO message_content (
                 message_id, reader_excerpt, truncated, body_byte_count, body_file_key
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(message_id) DO UPDATE SET
                 reader_excerpt = excluded.reader_excerpt,
                 truncated = excluded.truncated,
                 body_byte_count = excluded.body_byte_count,
                 body_file_key = excluded.body_file_key",
            params![
                reservation.message_id.get(),
                record.reader_excerpt.as_ref(),
                record.body_truncated,
                body_byte_count,
                record.body_file_key.as_ref().map(FileKey::as_str),
            ],
        )
        .map_err(DbFailure::database)?;

    for attachment in &record.attachments {
        let byte_count = database_byte_count(attachment.byte_count, "content attachment")?;
        transaction
            .execute(
                "INSERT INTO attachments (
                     message_id, ordinal, file_name, media_type, content_id,
                     disposition, byte_count, file_key
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(message_id, ordinal) DO UPDATE SET
                     file_name = excluded.file_name,
                     media_type = excluded.media_type,
                     content_id = excluded.content_id,
                     disposition = excluded.disposition,
                     byte_count = excluded.byte_count,
                     file_key = excluded.file_key",
                params![
                    reservation.message_id.get(),
                    i64::from(attachment.ordinal),
                    attachment.file_name.as_ref(),
                    attachment.media_type.as_ref(),
                    attachment.content_id.as_deref(),
                    attachment.disposition.as_ref(),
                    byte_count,
                    attachment.file_key.as_str(),
                ],
            )
            .map_err(DbFailure::database)?;
    }
    transaction
        .execute(
            "DELETE FROM attachments
             WHERE message_id = ?1 AND ordinal >= ?2",
            params![
                reservation.message_id.get(),
                i64::try_from(record.attachments.len())
                    .expect("validated attachment count fits in i64")
            ],
        )
        .map_err(DbFailure::database)?;

    let deleted = transaction
        .execute(
            "DELETE FROM file_staging
             WHERE batch_token = ?1
               AND message_id = ?2
               AND account_id = ?3
               AND content_generation = ?4",
            params![
                reservation.batch_token.encoded(),
                reservation.message_id.get(),
                reservation.account_id,
                reservation.generation,
            ],
        )
        .map_err(DbFailure::database)?;
    if deleted != reservation.manifest.file_count() {
        return Err(DbFailure::conflict(
            "content reservation changed while finalizing",
        ));
    }

    for file_key in old_file_keys {
        queue_file_if_unreferenced(&transaction, &file_key, now_ms)?;
    }

    // Once this hook runs, durable staging owns cleanup even if commit is uncertain.
    before_commit();
    transaction.commit().map_err(DbFailure::database)?;
    Ok(ContentFinalizeOutcome {
        generation: reservation.generation,
    })
}

pub(super) fn abort_content(
    connection: &mut Connection,
    reservation: &ContentReservation,
    now_ms: i64,
) -> Result<ContentAbortOutcome, DbFailure> {
    validate_timestamp(now_ms)?;
    reservation.manifest.validate()?;
    let transaction = immediate_transaction(connection)?;
    let rows = load_staged_rows(&transaction, reservation.batch_token)?;
    if rows.is_empty() {
        transaction.commit().map_err(DbFailure::database)?;
        return Ok(ContentAbortOutcome::AlreadyReleased);
    }
    verify_staged_rows(reservation, &rows)?;
    let deleted = transaction
        .execute(
            "DELETE FROM file_staging
             WHERE batch_token = ?1
               AND message_id = ?2
               AND account_id = ?3
               AND content_generation = ?4",
            params![
                reservation.batch_token.encoded(),
                reservation.message_id.get(),
                reservation.account_id,
                reservation.generation,
            ],
        )
        .map_err(DbFailure::database)?;
    if deleted != reservation.manifest.file_count() {
        return Err(DbFailure::conflict(
            "content reservation changed while aborting",
        ));
    }
    for file_key in reservation_file_keys(&reservation.manifest) {
        queue_file_if_unreferenced(&transaction, file_key.as_str(), now_ms)?;
    }
    transaction.commit().map_err(DbFailure::database)?;
    Ok(ContentAbortOutcome::Released)
}

fn reap_expired_reservations(transaction: &Transaction<'_>, now_ms: i64) -> Result<(), DbFailure> {
    let batch_tokens = {
        let mut statement = transaction
            .prepare(
                "SELECT batch_token
                 FROM file_staging
                 WHERE expires_at_ms <= ?1
                 GROUP BY batch_token
                 ORDER BY min(expires_at_ms), batch_token
                 LIMIT ?2",
            )
            .map_err(DbFailure::database)?;
        statement
            .query_map(
                params![
                    now_ms,
                    i64::try_from(MAX_EXPIRED_BATCHES_PER_RESERVE)
                        .expect("expired batch limit fits in i64")
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(DbFailure::database)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(DbFailure::database)?
    };

    for batch_token in batch_tokens {
        let file_keys = {
            let mut statement = transaction
                .prepare(
                    "SELECT file_key
                     FROM file_staging
                     WHERE batch_token = ?1
                     ORDER BY file_key
                     LIMIT 34",
                )
                .map_err(DbFailure::database)?;
            statement
                .query_map([&batch_token], |row| row.get::<_, String>(0))
                .map_err(DbFailure::database)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(DbFailure::database)?
        };
        if file_keys.is_empty() || file_keys.len() > MAX_CONTENT_FILES {
            return Err(DbFailure::resource_limit(
                "expired content reservation has an invalid file count",
            ));
        }
        let deleted = transaction
            .execute(
                "DELETE FROM file_staging
                 WHERE batch_token = ?1 AND expires_at_ms <= ?2",
                params![batch_token, now_ms],
            )
            .map_err(DbFailure::database)?;
        if deleted != file_keys.len() {
            return Err(DbFailure::conflict(
                "expired content reservation changed while reaping",
            ));
        }
        for file_key in file_keys {
            queue_file_if_unreferenced(transaction, &file_key, now_ms)?;
        }
    }
    Ok(())
}

fn insert_staged_file(
    transaction: &Transaction<'_>,
    encoded_token: &str,
    request: &ReserveContentRequest,
    generation: i64,
    file_key: &FileKey,
    file_kind: &str,
    part_ordinal: Option<i64>,
) -> Result<(), DbFailure> {
    transaction
        .execute(
            "INSERT INTO file_staging (
                 file_key, batch_token, message_id, account_id, content_generation,
                 file_kind, part_ordinal, created_at_ms, expires_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                file_key.as_str(),
                encoded_token,
                request.message_id.get(),
                request.account_id,
                generation,
                file_kind,
                part_ordinal,
                request.created_at_ms,
                request.expires_at_ms,
            ],
        )
        .map_err(DbFailure::database)?;
    Ok(())
}

fn immediate_transaction(connection: &mut Connection) -> Result<Transaction<'_>, DbFailure> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)
}

fn load_message_identity(
    transaction: &Transaction<'_>,
    message_id: MessageId,
) -> Result<Option<(i64, i64)>, DbFailure> {
    transaction
        .query_row(
            "SELECT account_id, content_generation FROM messages WHERE id = ?1",
            [message_id.get()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(DbFailure::database)
}

fn verify_current_generation(
    transaction: &Transaction<'_>,
    reservation: &ContentReservation,
) -> Result<(), DbFailure> {
    let Some((account_id, generation)) =
        load_message_identity(transaction, reservation.message_id)?
    else {
        return Err(DbFailure::not_found("message no longer exists"));
    };
    if account_id != reservation.account_id || generation != reservation.generation {
        return Err(DbFailure::conflict(
            "content reservation was fenced by a newer generation",
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct StagedRow {
    file_key: Box<str>,
    message_id: i64,
    account_id: i64,
    generation: i64,
    file_kind: Box<str>,
    part_ordinal: Option<i64>,
    created_at_ms: i64,
    expires_at_ms: i64,
}

fn load_staged_rows(
    transaction: &Transaction<'_>,
    batch_token: ContentBatchToken,
) -> Result<Vec<StagedRow>, DbFailure> {
    let encoded_token = batch_token.encoded();
    let mut statement = transaction
        .prepare(
            "SELECT file_key, message_id, account_id, content_generation,
                    file_kind, part_ordinal, created_at_ms, expires_at_ms
             FROM file_staging
             WHERE batch_token = ?1
             ORDER BY file_kind, part_ordinal, file_key
             LIMIT 34",
        )
        .map_err(DbFailure::database)?;
    let rows = statement
        .query_map([encoded_token], |row| {
            Ok(StagedRow {
                file_key: row.get::<_, String>(0)?.into_boxed_str(),
                message_id: row.get(1)?,
                account_id: row.get(2)?,
                generation: row.get(3)?,
                file_kind: row.get::<_, String>(4)?.into_boxed_str(),
                part_ordinal: row.get(5)?,
                created_at_ms: row.get(6)?,
                expires_at_ms: row.get(7)?,
            })
        })
        .map_err(DbFailure::database)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(DbFailure::database)?;
    if rows.len() > MAX_CONTENT_FILES {
        return Err(DbFailure::resource_limit(format!(
            "content reservation exceeds the {MAX_CONTENT_FILES}-file limit"
        )));
    }
    Ok(rows)
}

fn verify_persisted_reservation(
    transaction: &Transaction<'_>,
    reservation: &ContentReservation,
) -> Result<(), DbFailure> {
    let rows = load_staged_rows(transaction, reservation.batch_token)?;
    if rows.is_empty() {
        return Err(DbFailure::conflict("content reservation no longer exists"));
    }
    verify_staged_rows(reservation, &rows)
}

fn verify_staged_rows(
    reservation: &ContentReservation,
    rows: &[StagedRow],
) -> Result<(), DbFailure> {
    if rows.len() != reservation.manifest.file_count() {
        return Err(DbFailure::conflict(
            "content reservation file manifest is incomplete",
        ));
    }
    let mut saw_body = false;
    let mut saw_attachments = [false; MAX_ATTACHMENTS];
    for row in rows {
        if row.message_id != reservation.message_id.get()
            || row.account_id != reservation.account_id
            || row.generation != reservation.generation
            || row.created_at_ms != reservation.created_at_ms
            || row.expires_at_ms != reservation.expires_at_ms
        {
            return Err(DbFailure::conflict(
                "content reservation identity does not match",
            ));
        }
        match row.file_kind.as_ref() {
            "body" => {
                if saw_body
                    || row.part_ordinal.is_some()
                    || reservation.manifest.body_file_key().map(FileKey::as_str)
                        != Some(row.file_key.as_ref())
                {
                    return Err(DbFailure::conflict(
                        "content reservation body manifest does not match",
                    ));
                }
                saw_body = true;
            }
            "attachment" => {
                let Some(ordinal) = row
                    .part_ordinal
                    .and_then(|ordinal| usize::try_from(ordinal).ok())
                    .filter(|ordinal| *ordinal < MAX_ATTACHMENTS)
                else {
                    return Err(DbFailure::conflict(
                        "content reservation attachment ordinal is invalid",
                    ));
                };
                if saw_attachments[ordinal]
                    || reservation.manifest.attachments().get(ordinal).map(
                        |(manifest_ordinal, key)| {
                            usize::from(*manifest_ordinal) == ordinal
                                && key.as_str() == row.file_key.as_ref()
                        },
                    ) != Some(true)
                {
                    return Err(DbFailure::conflict(
                        "content reservation attachment manifest does not match",
                    ));
                }
                saw_attachments[ordinal] = true;
            }
            _ => {
                return Err(DbFailure::conflict(
                    "content reservation contains an invalid file kind",
                ));
            }
        }
    }
    if saw_body != reservation.manifest.body_file_key().is_some()
        || saw_attachments[..reservation.manifest.attachments().len()]
            .iter()
            .any(|seen| !seen)
    {
        return Err(DbFailure::conflict(
            "content reservation file manifest is incomplete",
        ));
    }
    Ok(())
}

fn load_existing_file_keys(
    transaction: &Transaction<'_>,
    message_id: MessageId,
) -> Result<Vec<Box<str>>, DbFailure> {
    let mut file_keys = Vec::with_capacity(MAX_CONTENT_FILES);
    let body_file_key: Option<String> = transaction
        .query_row(
            "SELECT body_file_key FROM message_content WHERE message_id = ?1",
            [message_id.get()],
            |row| row.get(0),
        )
        .optional()
        .map_err(DbFailure::database)?
        .flatten();
    if let Some(file_key) = body_file_key {
        file_keys.push(file_key.into_boxed_str());
    }

    let mut statement = transaction
        .prepare(
            "SELECT file_key FROM attachments
             WHERE message_id = ?1 AND file_key IS NOT NULL
             ORDER BY ordinal
             LIMIT 33",
        )
        .map_err(DbFailure::database)?;
    let attachment_keys = statement
        .query_map([message_id.get()], |row| row.get::<_, String>(0))
        .map_err(DbFailure::database)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(DbFailure::database)?;
    if attachment_keys.len() > MAX_ATTACHMENTS {
        return Err(DbFailure::resource_limit(format!(
            "stored content exceeds the {MAX_ATTACHMENTS}-attachment limit"
        )));
    }
    file_keys.extend(attachment_keys.into_iter().map(String::into_boxed_str));
    Ok(file_keys)
}

fn queue_file_if_unreferenced(
    transaction: &Transaction<'_>,
    file_key: &str,
    queued_at_ms: i64,
) -> Result<(), DbFailure> {
    transaction
        .execute(
            "INSERT OR IGNORE INTO file_gc (file_key, queued_at_ms)
             SELECT ?1, ?2
             WHERE NOT EXISTS (
                       SELECT 1 FROM message_content WHERE body_file_key = ?1
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM attachments WHERE file_key = ?1
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM outbox WHERE mime_file_key = ?1
                   )",
            params![file_key, queued_at_ms],
        )
        .map_err(DbFailure::database)?;
    Ok(())
}

fn reservation_file_keys(manifest: &ContentManifest) -> impl Iterator<Item = &FileKey> {
    manifest
        .body_file_key()
        .into_iter()
        .chain(manifest.attachments().iter().map(|(_, key)| key))
}

fn validate_record(record: &ContentRecord) -> Result<(), DbFailure> {
    validate_bounded_text(&record.subject, MAX_SUBJECT_BYTES, true, "subject")?;
    validate_bounded_text(&record.sender_name, MAX_ADDRESS_BYTES, true, "sender name")?;
    validate_bounded_text(
        &record.sender_address,
        MAX_ADDRESS_BYTES,
        true,
        "sender address",
    )?;
    validate_bounded_text(&record.preview, MAX_PREVIEW_BYTES, true, "preview")?;
    validate_bounded_text(
        &record.reader_excerpt,
        MAX_READER_EXCERPT_BYTES,
        true,
        "reader excerpt",
    )?;
    if let Some(received_at_ms) = record.received_at_ms {
        validate_timestamp(received_at_ms)?;
    }
    database_byte_count(record.body_byte_count, "content body")?;
    ContentManifest::from_record(record)?;
    for attachment in &record.attachments {
        validate_bounded_text(
            &attachment.file_name,
            MAX_FILE_NAME_BYTES,
            true,
            "attachment file name",
        )?;
        validate_bounded_text(
            &attachment.media_type,
            MAX_MEDIA_TYPE_BYTES,
            false,
            "attachment media type",
        )?;
        if let Some(content_id) = &attachment.content_id {
            validate_bounded_text(content_id, MAX_CONTENT_ID_BYTES, false, "content id")?;
        }
        validate_bounded_text(
            &attachment.disposition,
            MAX_DISPOSITION_BYTES,
            false,
            "attachment disposition",
        )?;
        database_byte_count(attachment.byte_count, "content attachment")?;
    }
    Ok(())
}

fn validate_bounded_text(
    value: &str,
    maximum: usize,
    allow_empty: bool,
    field: &str,
) -> Result<(), DbFailure> {
    let byte_count = value.len();
    if byte_count > maximum || (!allow_empty && byte_count == 0) {
        return Err(DbFailure::resource_limit(format!(
            "{field} is outside its SQLite byte limit"
        )));
    }
    Ok(())
}

fn database_byte_count(value: u64, field: &str) -> Result<i64, DbFailure> {
    if value > MAX_BODY_BYTES {
        return Err(DbFailure::resource_limit(format!(
            "{field} byte count exceeds the SQLite limit"
        )));
    }
    i64::try_from(value).map_err(|_| DbFailure::resource_limit("content byte count overflow"))
}

fn validate_account_id(account_id: i64) -> Result<(), DbFailure> {
    if account_id > 0 {
        Ok(())
    } else {
        Err(DbFailure::resource_limit(
            "content reservation account id must be positive",
        ))
    }
}

fn validate_lease(created_at_ms: i64, expires_at_ms: i64) -> Result<(), DbFailure> {
    validate_timestamp(created_at_ms)?;
    validate_timestamp(expires_at_ms)?;
    let ttl_ms = expires_at_ms
        .checked_sub(created_at_ms)
        .ok_or_else(|| DbFailure::resource_limit("content reservation TTL overflow"))?;
    if !(1..=MAX_STAGING_TTL_MS).contains(&ttl_ms) {
        return Err(DbFailure::resource_limit(format!(
            "content reservation TTL must be between 1 and {MAX_STAGING_TTL_MS} ms"
        )));
    }
    Ok(())
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
    use std::{cell::Cell, rc::Rc};

    use rusqlite::{Connection, params};

    use super::*;
    use crate::{
        content::AttachmentRecord,
        store::sqlite::{domain::FailureKind, migrations::migrate},
    };

    const NOW_MS: i64 = 1_000_000;

    fn database() -> Connection {
        let mut connection = Connection::open_in_memory().expect("open in-memory database");
        migrate(&mut connection).expect("migrate database");
        connection
            .execute(
                "INSERT INTO accounts (
                     id, provider, remote_key, name, address, state, accent_rgb
                 ) VALUES (1, 'imap', 'account-1', 'Personal',
                           'one@example.test', 'active', 0)",
                [],
            )
            .expect("insert account");
        connection
            .execute(
                "INSERT INTO messages (
                     id, account_id, remote_key, subject, received_at_ms
                 ) VALUES (1, 1, 'message-1', 'old subject', 1)",
                [],
            )
            .expect("insert message");
        connection
    }

    fn message_id() -> MessageId {
        MessageId::new(1).expect("positive message id")
    }

    fn token(value: u128) -> ContentBatchToken {
        ContentBatchToken::new(value.to_be_bytes())
    }

    fn body_key(value: u128) -> FileKey {
        FileKey::parse(&format!("body/{value:032x}.txt")).expect("valid body key")
    }

    fn attachment_key(value: u128) -> FileKey {
        FileKey::parse(&format!("attachment/{value:032x}.bin")).expect("valid attachment key")
    }

    fn record(version: u128, attachment_count: usize) -> ContentRecord {
        ContentRecord {
            subject: format!("subject {version}").into_boxed_str(),
            sender_name: "Sender".into(),
            sender_address: "sender@example.test".into(),
            received_at_ms: Some(NOW_MS),
            preview: format!("preview {version}").into_boxed_str(),
            reader_excerpt: format!("reader {version}").into_boxed_str(),
            body_truncated: false,
            body_byte_count: 100 + u64::try_from(version).expect("small test version"),
            body_file_key: Some(body_key(version)),
            attachments: (0..attachment_count)
                .map(|ordinal| AttachmentRecord {
                    ordinal: u16::try_from(ordinal).expect("bounded ordinal"),
                    file_name: format!("file-{ordinal}.bin").into_boxed_str(),
                    media_type: "application/octet-stream".into(),
                    content_id: None,
                    disposition: "attachment".into(),
                    byte_count: 10,
                    file_key: attachment_key(version * 100 + ordinal as u128 + 1),
                })
                .collect(),
        }
    }

    fn request(record: &ContentRecord, batch_token: ContentBatchToken) -> ReserveContentRequest {
        request_at(record, batch_token, NOW_MS, NOW_MS + 60_000)
    }

    fn request_at(
        record: &ContentRecord,
        batch_token: ContentBatchToken,
        created_at_ms: i64,
        expires_at_ms: i64,
    ) -> ReserveContentRequest {
        ReserveContentRequest::new(
            message_id(),
            1,
            batch_token,
            ContentManifest::from_record(record).expect("valid test manifest"),
            created_at_ms,
            expires_at_ms,
        )
        .expect("valid reserve request")
    }

    #[test]
    fn reserve_advances_generation_and_persists_exact_manifest() {
        let mut connection = database();
        let content = record(1, 2);
        let reservation =
            reserve_content(&mut connection, request(&content, token(1))).expect("reserve files");

        assert_eq!(reservation.generation(), 1);
        assert_eq!(reservation.message_id(), message_id());
        assert_eq!(reservation.account_id(), 1);
        assert_eq!(reservation.batch_token(), token(1));
        assert_eq!(reservation.manifest().file_count(), 3);
        let rows: i64 = connection
            .query_row("SELECT count(*) FROM file_staging", [], |row| row.get(0))
            .expect("count staging rows");
        let generation: i64 = connection
            .query_row(
                "SELECT content_generation FROM messages WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .expect("read generation");
        assert_eq!((rows, generation), (3, 1));
    }

    #[test]
    fn reserve_rejects_missing_or_wrong_account_and_generation_overflow() {
        let mut connection = database();
        let content = record(1, 0);
        let missing = ReserveContentRequest::new(
            MessageId::new(2).unwrap(),
            1,
            token(1),
            ContentManifest::from_record(&content).unwrap(),
            NOW_MS,
            NOW_MS + 1,
        )
        .unwrap();
        assert_eq!(
            reserve_content(&mut connection, missing).unwrap_err().kind,
            FailureKind::NotFound
        );

        let wrong_account = ReserveContentRequest::new(
            message_id(),
            2,
            token(2),
            ContentManifest::from_record(&content).unwrap(),
            NOW_MS,
            NOW_MS + 1,
        )
        .unwrap();
        assert_eq!(
            reserve_content(&mut connection, wrong_account)
                .unwrap_err()
                .kind,
            FailureKind::Conflict
        );

        connection
            .execute(
                "UPDATE messages SET content_generation = ?1 WHERE id = 1",
                [i64::MAX],
            )
            .unwrap();
        assert_eq!(
            reserve_content(&mut connection, request(&content, token(3)))
                .unwrap_err()
                .kind,
            FailureKind::ResourceLimit
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM file_staging", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn newer_reservation_fences_older_generation_without_mutating_its_lease() {
        let mut connection = database();
        let first_content = record(1, 0);
        let first = reserve_content(&mut connection, request(&first_content, token(1))).unwrap();
        let first_lease: (i64, i64) = connection
            .query_row(
                "SELECT created_at_ms, expires_at_ms
                 FROM file_staging WHERE batch_token = ?1 LIMIT 1",
                [token(1).encoded()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        let second_content = record(2, 0);
        let second = reserve_content(&mut connection, request(&second_content, token(2))).unwrap();
        assert_eq!(second.generation(), 2);
        assert_eq!(
            finalize_content(&mut connection, &first, &first_content, NOW_MS + 1)
                .unwrap_err()
                .kind,
            FailureKind::Conflict
        );
        let persisted_lease: (i64, i64) = connection
            .query_row(
                "SELECT created_at_ms, expires_at_ms
                 FROM file_staging WHERE batch_token = ?1 LIMIT 1",
                [token(1).encoded()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(persisted_lease, first_lease);
    }

    #[test]
    fn reserve_reaps_only_expired_batches_and_queues_their_files() {
        let mut connection = database();
        let expired_content = record(1, 0);
        reserve_content(
            &mut connection,
            request_at(&expired_content, token(1), NOW_MS - 100, NOW_MS - 1),
        )
        .unwrap();
        let active_content = record(2, 0);
        reserve_content(
            &mut connection,
            request_at(&active_content, token(2), NOW_MS - 50, NOW_MS + 60_000),
        )
        .unwrap();

        reserve_content(&mut connection, request(&record(3, 0), token(3))).unwrap();

        let batch_counts = connection
            .query_row(
                "SELECT
                     count(*) FILTER (WHERE batch_token = ?1),
                     count(*) FILTER (WHERE batch_token = ?2),
                     count(*) FILTER (WHERE batch_token = ?3)
                 FROM file_staging",
                params![token(1).encoded(), token(2).encoded(), token(3).encoded()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(batch_counts, (0, 1, 1));
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM file_gc WHERE file_key = ?1",
                    [body_key(1).as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM file_gc WHERE file_key = ?1",
                    [body_key(2).as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn finalize_replaces_v1_with_v2_preserves_remote_keys_and_queues_old_files() {
        let mut connection = database();
        let first_content = record(1, 2);
        let first = reserve_content(&mut connection, request(&first_content, token(1))).unwrap();
        finalize_content(&mut connection, &first, &first_content, NOW_MS + 1).unwrap();
        connection
            .execute(
                "UPDATE attachments SET remote_key = 'remote-zero'
                 WHERE message_id = 1 AND ordinal = 0",
                [],
            )
            .unwrap();

        let second_content = record(2, 1);
        let second = reserve_content(&mut connection, request(&second_content, token(2))).unwrap();
        let outcome =
            finalize_content(&mut connection, &second, &second_content, NOW_MS + 2).unwrap();
        assert_eq!(outcome.generation, 2);

        let stored: (String, String, i64, String, Option<String>) = connection
            .query_row(
                "SELECT m.subject, m.preview, m.has_attachment,
                        c.body_file_key, a.remote_key
                 FROM messages AS m
                 JOIN message_content AS c ON c.message_id = m.id
                 JOIN attachments AS a ON a.message_id = m.id AND a.ordinal = 0
                 WHERE m.id = 1",
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
            .unwrap();
        assert_eq!(
            stored,
            (
                "subject 2".to_owned(),
                "preview 2".to_owned(),
                1,
                body_key(2).as_str().to_owned(),
                Some("remote-zero".to_owned()),
            )
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM attachments WHERE message_id = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );

        let queued = connection
            .prepare("SELECT file_key FROM file_gc ORDER BY file_key")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let mut expected = vec![
            body_key(1).as_str().to_owned(),
            attachment_key(101).as_str().to_owned(),
            attachment_key(102).as_str().to_owned(),
        ];
        expected.sort();
        assert_eq!(queued, expected);
    }

    #[test]
    fn finalize_sql_failure_rolls_back_and_does_not_run_commit_hook() {
        let mut connection = database();
        let content = record(1, 1);
        let reservation = reserve_content(&mut connection, request(&content, token(1))).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_content_attachment
                 BEFORE INSERT ON attachments
                 BEGIN
                     SELECT RAISE(ABORT, 'injected attachment failure');
                 END;",
            )
            .unwrap();
        let hook_calls = Rc::new(Cell::new(0));
        let hook_counter = Rc::clone(&hook_calls);

        let failure = finalize_content_with_commit_hook(
            &mut connection,
            &reservation,
            &content,
            NOW_MS + 1,
            move || hook_counter.set(hook_counter.get() + 1),
        )
        .unwrap_err();
        assert_eq!(failure.kind, FailureKind::Database);
        assert_eq!(hook_calls.get(), 0);
        assert_eq!(
            connection
                .query_row(
                    "SELECT subject, content_generation FROM messages WHERE id = 1",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap(),
            ("old subject".to_owned(), 1)
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM file_staging", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn commit_failure_runs_hook_once_and_rolls_back_database_changes() {
        let mut connection = database();
        let content = record(1, 0);
        let reservation = reserve_content(&mut connection, request(&content, token(1))).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER inject_deferred_foreign_key_failure
                 AFTER UPDATE OF subject ON messages
                 BEGIN
                     INSERT INTO message_content (message_id) VALUES (999999);
                 END;",
            )
            .unwrap();
        connection
            .pragma_update(None, "defer_foreign_keys", true)
            .unwrap();
        let hook_calls = Rc::new(Cell::new(0));
        let hook_counter = Rc::clone(&hook_calls);

        let failure = finalize_content_with_commit_hook(
            &mut connection,
            &reservation,
            &content,
            NOW_MS + 1,
            move || hook_counter.set(hook_counter.get() + 1),
        )
        .unwrap_err();
        assert_eq!(failure.kind, FailureKind::Database);
        assert_eq!(hook_calls.get(), 1);
        assert_eq!(
            connection
                .query_row("SELECT subject FROM messages WHERE id = 1", [], |row| {
                    row.get::<_, String>(0)
                },)
                .unwrap(),
            "old subject"
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM file_staging", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn abort_only_releases_the_exact_batch_and_is_idempotent() {
        let mut connection = database();
        let first_content = record(1, 0);
        let first = reserve_content(&mut connection, request(&first_content, token(1))).unwrap();
        let second_content = record(2, 0);
        let second = reserve_content(&mut connection, request(&second_content, token(2))).unwrap();

        assert_eq!(
            abort_content(&mut connection, &first, NOW_MS + 1).unwrap(),
            ContentAbortOutcome::Released
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM file_staging WHERE batch_token = ?1",
                    [token(2).encoded()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            abort_content(&mut connection, &first, NOW_MS + 2).unwrap(),
            ContentAbortOutcome::AlreadyReleased
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM file_gc WHERE file_key = ?1",
                    [body_key(1).as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(second.generation(), 2);
    }

    #[test]
    fn attachment_and_staging_limits_accept_n_and_reject_n_plus_one() {
        let maximum_record = record(1, MAX_ATTACHMENTS);
        assert_eq!(
            ContentManifest::from_record(&maximum_record)
                .unwrap()
                .file_count(),
            MAX_CONTENT_FILES
        );
        let oversized_record = record(2, MAX_ATTACHMENTS + 1);
        assert_eq!(
            ContentManifest::from_record(&oversized_record)
                .unwrap_err()
                .kind,
            FailureKind::ResourceLimit
        );
        assert_eq!(
            ContentManifest::new(None, vec![(0, attachment_key(99))])
                .unwrap_err()
                .kind,
            FailureKind::ResourceLimit
        );

        let mut connection = database();
        for index in 0..(MAX_STAGED_FILES - 1) {
            let file_key = attachment_key(10_000 + index as u128);
            connection
                .execute(
                    "INSERT INTO file_staging (
                         file_key, batch_token, message_id, account_id, content_generation,
                         file_kind, part_ordinal, created_at_ms, expires_at_ms
                     ) VALUES (?1, ?2, ?3, 1, ?3, 'attachment', 0, ?4, ?5)",
                    params![
                        file_key.as_str(),
                        token(10_000 + index as u128).encoded(),
                        i64::try_from(index + 2).unwrap(),
                        NOW_MS,
                        NOW_MS + 60_000,
                    ],
                )
                .unwrap();
        }
        let content = record(3, 0);
        reserve_content(&mut connection, request(&content, token(3)))
            .expect("the 256th staged file is accepted");
        connection
            .execute(
                "UPDATE messages SET content_generation = 0 WHERE id = 1",
                [],
            )
            .unwrap();
        assert_eq!(
            reserve_content(&mut connection, request(&record(4, 0), token(4)))
                .unwrap_err()
                .kind,
            FailureKind::ResourceLimit
        );
    }

    #[test]
    fn capacity_failure_commits_bounded_expired_batch_recovery() {
        let mut connection = database();
        for index in 0..MAX_STAGED_FILES {
            let file_key = attachment_key(20_000 + index as u128);
            connection
                .execute(
                    "INSERT INTO file_staging (
                         file_key, batch_token, message_id, account_id, content_generation,
                         file_kind, part_ordinal, created_at_ms, expires_at_ms
                     ) VALUES (?1, ?2, ?3, 1, ?3, 'attachment', 0, ?4, ?5)",
                    params![
                        file_key.as_str(),
                        token(20_000 + index as u128).encoded(),
                        i64::try_from(index + 2).unwrap(),
                        NOW_MS - 100,
                        NOW_MS - 1,
                    ],
                )
                .unwrap();
        }

        assert_eq!(
            reserve_content(
                &mut connection,
                request(&record(1, MAX_ATTACHMENTS), token(1)),
            )
            .unwrap_err()
            .kind,
            FailureKind::ResourceLimit
        );
        assert_eq!(
            connection
                .query_row("SELECT file_count FROM file_staging_usage", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            i64::try_from(MAX_STAGED_FILES - MAX_EXPIRED_BATCHES_PER_RESERVE).unwrap()
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM file_gc", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            i64::try_from(MAX_EXPIRED_BATCHES_PER_RESERVE).unwrap()
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT content_generation FROM messages WHERE id = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn ttl_and_text_limits_accept_n_and_reject_n_plus_one() {
        let content = record(1, 0);
        ReserveContentRequest::new(
            message_id(),
            1,
            token(1),
            ContentManifest::from_record(&content).unwrap(),
            NOW_MS,
            NOW_MS + MAX_STAGING_TTL_MS,
        )
        .expect("maximum TTL is accepted");
        assert_eq!(
            ReserveContentRequest::new(
                message_id(),
                1,
                token(2),
                ContentManifest::from_record(&content).unwrap(),
                NOW_MS,
                NOW_MS + MAX_STAGING_TTL_MS + 1,
            )
            .unwrap_err()
            .kind,
            FailureKind::ResourceLimit
        );

        let mut oversized = content;
        oversized.preview = "x".repeat(MAX_PREVIEW_BYTES + 1).into_boxed_str();
        let mut connection = database();
        let reservation = reserve_content(&mut connection, request(&oversized, token(3))).unwrap();
        assert_eq!(
            finalize_content(&mut connection, &reservation, &oversized, NOW_MS + 1)
                .unwrap_err()
                .kind,
            FailureKind::ResourceLimit
        );
        assert_eq!(
            connection
                .query_row("SELECT subject FROM messages WHERE id = 1", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "old subject"
        );
    }

    #[test]
    fn expired_or_mismatched_reservation_cannot_finalize() {
        let mut connection = database();
        let content = record(1, 1);
        let reservation = reserve_content(&mut connection, request(&content, token(1))).unwrap();

        let mut wrong_manifest = reservation.clone();
        wrong_manifest.manifest = ContentManifest::from_record(&record(2, 1)).unwrap();
        assert_eq!(
            finalize_content(&mut connection, &wrong_manifest, &record(2, 1), NOW_MS + 1,)
                .unwrap_err()
                .kind,
            FailureKind::Conflict
        );
        assert_eq!(
            finalize_content(&mut connection, &reservation, &content, NOW_MS + 60_000,)
                .unwrap_err()
                .kind,
            FailureKind::Conflict
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM file_staging", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            2
        );
    }
}
