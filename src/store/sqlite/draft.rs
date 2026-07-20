use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::content::FileKey;

use super::{
    account::{AccountGeneration, AccountLifecycle},
    domain::{AccountId, DbFailure, MessageId},
};

pub(crate) const MAX_DRAFT_BODY_BYTES: u64 = 1024 * 1024;
const MAX_DRAFT_RECIPIENTS: usize = 64;
const MAX_ADDRESS_BYTES: usize = 320;
const MAX_DISPLAY_NAME_BYTES: usize = 320;
const MAX_SUBJECT_BYTES: usize = 998;
const MAX_PREVIEW_BYTES: usize = 2 * 1024;
const MAX_READER_EXCERPT_BYTES: usize = 64 * 1024;
const MAX_REMOTE_KEY_BYTES: usize = 512;
const MIN_TIMESTAMP_MS: i64 = -62_135_596_800_000;
const MAX_TIMESTAMP_MS: i64 = 253_402_300_799_999;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DraftRecipient {
    pub(crate) address: Box<str>,
    pub(crate) display_name: Box<str>,
}

impl DraftRecipient {
    pub(crate) fn new(address: &str, display_name: &str) -> Result<Self, DbFailure> {
        validate_text(address, MAX_ADDRESS_BYTES, false, "draft recipient address")?;
        validate_text(
            display_name,
            MAX_DISPLAY_NAME_BYTES,
            true,
            "draft recipient name",
        )?;
        Ok(Self {
            address: address.into(),
            display_name: display_name.into(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NewDraft {
    account_id: AccountId,
    expected_generation: AccountGeneration,
    remote_key: Box<str>,
    subject: Box<str>,
    preview: Box<str>,
    reader_excerpt: Box<str>,
    body_file_key: FileKey,
    body_byte_count: u64,
    recipients: Box<[DraftRecipient]>,
    now_ms: i64,
}

impl NewDraft {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        account_id: AccountId,
        expected_generation: AccountGeneration,
        remote_key: &str,
        subject: &str,
        preview: &str,
        reader_excerpt: &str,
        body_file_key: FileKey,
        body_byte_count: u64,
        recipients: Vec<DraftRecipient>,
        now_ms: i64,
    ) -> Result<Self, DbFailure> {
        validate_text(remote_key, MAX_REMOTE_KEY_BYTES, false, "draft local key")?;
        validate_text(subject, MAX_SUBJECT_BYTES, true, "draft subject")?;
        validate_text(preview, MAX_PREVIEW_BYTES, true, "draft preview")?;
        validate_text(
            reader_excerpt,
            MAX_READER_EXCERPT_BYTES,
            true,
            "draft reader excerpt",
        )?;
        validate_body(&body_file_key, body_byte_count)?;
        validate_recipients(&recipients)?;
        validate_timestamp(now_ms)?;
        Ok(Self {
            account_id,
            expected_generation,
            remote_key: remote_key.into(),
            subject: subject.into(),
            preview: preview.into(),
            reader_excerpt: reader_excerpt.into(),
            body_file_key,
            body_byte_count,
            recipients: recipients.into_boxed_slice(),
            now_ms,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DraftUpdate {
    message_id: MessageId,
    account_id: AccountId,
    expected_generation: AccountGeneration,
    expected_revision: u64,
    subject: Box<str>,
    preview: Box<str>,
    reader_excerpt: Box<str>,
    body_file_key: FileKey,
    body_byte_count: u64,
    recipients: Box<[DraftRecipient]>,
    now_ms: i64,
}

impl DraftUpdate {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        message_id: MessageId,
        account_id: AccountId,
        expected_generation: AccountGeneration,
        expected_revision: u64,
        subject: &str,
        preview: &str,
        reader_excerpt: &str,
        body_file_key: FileKey,
        body_byte_count: u64,
        recipients: Vec<DraftRecipient>,
        now_ms: i64,
    ) -> Result<Self, DbFailure> {
        if expected_revision > i64::MAX as u64 {
            return Err(DbFailure::resource_limit(
                "draft revision is outside SQLite bounds",
            ));
        }
        validate_text(subject, MAX_SUBJECT_BYTES, true, "draft subject")?;
        validate_text(preview, MAX_PREVIEW_BYTES, true, "draft preview")?;
        validate_text(
            reader_excerpt,
            MAX_READER_EXCERPT_BYTES,
            true,
            "draft reader excerpt",
        )?;
        validate_body(&body_file_key, body_byte_count)?;
        validate_recipients(&recipients)?;
        validate_timestamp(now_ms)?;
        Ok(Self {
            message_id,
            account_id,
            expected_generation,
            expected_revision,
            subject: subject.into(),
            preview: preview.into(),
            reader_excerpt: reader_excerpt.into(),
            body_file_key,
            body_byte_count,
            recipients: recipients.into_boxed_slice(),
            now_ms,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DraftSnapshot {
    pub(crate) message_id: MessageId,
    pub(crate) account_id: AccountId,
    pub(crate) revision: u64,
    pub(crate) subject: Box<str>,
    pub(crate) preview: Box<str>,
    pub(crate) reader_excerpt: Box<str>,
    pub(crate) body_file_key: FileKey,
    pub(crate) body_byte_count: u64,
    pub(crate) recipients: Box<[DraftRecipient]>,
    pub(crate) updated_at_ms: i64,
    pub(crate) locked_artifact_generation: Option<u64>,
}

pub(crate) fn create_draft(
    connection: &mut Connection,
    draft: &NewDraft,
) -> Result<DraftSnapshot, DbFailure> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    let sender = require_account_fence(&transaction, draft.account_id, draft.expected_generation)?;
    require_clean_stats(&transaction, draft.account_id)?;
    let folder_id = ensure_drafts_folder(&transaction, draft.account_id)?;
    transaction
        .execute(
            "INSERT INTO messages
                 (account_id, remote_key, sender_name, sender_address, subject, preview,
                  received_at_ms, unread, starred, has_attachment, revision)
             VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, 0, 0, 0, 0)",
            params![
                draft.account_id.get(),
                draft.remote_key.as_ref(),
                sender,
                draft.subject.as_ref(),
                draft.preview.as_ref(),
                draft.now_ms,
            ],
        )
        .map_err(map_write_error)?;
    let message_id = MessageId::new(transaction.last_insert_rowid())
        .map_err(|error| DbFailure::database(error.to_string()))?;
    transaction
        .execute(
            "INSERT INTO message_folders (message_id, folder_id, account_id)
             VALUES (?1, ?2, ?3)",
            params![message_id.get(), folder_id, draft.account_id.get()],
        )
        .map_err(map_write_error)?;
    transaction
        .execute(
            "INSERT INTO message_content
                 (message_id, reader_excerpt, truncated, body_byte_count, body_file_key)
             VALUES (?1, ?2, 0, ?3, ?4)",
            params![
                message_id.get(),
                draft.reader_excerpt.as_ref(),
                draft.body_byte_count as i64,
                draft.body_file_key.as_str(),
            ],
        )
        .map_err(map_write_error)?;
    transaction
        .execute(
            "INSERT INTO local_drafts (message_id, updated_at_ms) VALUES (?1, ?2)",
            params![message_id.get(), draft.now_ms],
        )
        .map_err(map_write_error)?;
    replace_recipients(&transaction, message_id, &draft.recipients)?;
    record_draft_created(&transaction, draft.account_id)?;
    let snapshot = load_draft_from(&transaction, message_id)?
        .ok_or_else(|| DbFailure::database("created draft could not be loaded"))?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(snapshot)
}

pub(crate) fn update_draft(
    connection: &mut Connection,
    draft: &DraftUpdate,
) -> Result<DraftSnapshot, DbFailure> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    require_account_fence(&transaction, draft.account_id, draft.expected_generation)?;
    let current: Option<(i64, Option<i64>, Option<String>)> = transaction
        .query_row(
            "SELECT message.revision, local.locked_artifact_generation, content.body_file_key
             FROM messages AS message
             JOIN local_drafts AS local ON local.message_id = message.id
             JOIN message_content AS content ON content.message_id = message.id
             WHERE message.id = ?1 AND message.account_id = ?2",
            params![draft.message_id.get(), draft.account_id.get()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((revision, lock, previous_body)) = current else {
        return Err(DbFailure::not_found("draft no longer exists"));
    };
    if lock.is_some() {
        return Err(DbFailure::conflict(
            "draft is locked by an outbound reservation",
        ));
    }
    if revision != draft.expected_revision as i64 {
        return Err(DbFailure::conflict("draft revision changed"));
    }
    let updated = transaction
        .execute(
            "UPDATE messages
             SET subject = ?3, preview = ?4, received_at_ms = ?5, revision = revision + 1
             WHERE id = ?1 AND account_id = ?2 AND revision = ?6
               AND revision < 9223372036854775807",
            params![
                draft.message_id.get(),
                draft.account_id.get(),
                draft.subject.as_ref(),
                draft.preview.as_ref(),
                draft.now_ms,
                revision,
            ],
        )
        .map_err(map_write_error)?;
    if updated != 1 {
        return Err(DbFailure::conflict("draft revision changed"));
    }
    transaction
        .execute(
            "UPDATE message_content
             SET reader_excerpt = ?2, truncated = 0,
                 body_byte_count = ?3, body_file_key = ?4
             WHERE message_id = ?1",
            params![
                draft.message_id.get(),
                draft.reader_excerpt.as_ref(),
                draft.body_byte_count as i64,
                draft.body_file_key.as_str(),
            ],
        )
        .map_err(map_write_error)?;
    transaction
        .execute(
            "UPDATE local_drafts SET updated_at_ms = ?2 WHERE message_id = ?1",
            params![draft.message_id.get(), draft.now_ms],
        )
        .map_err(map_write_error)?;
    replace_recipients(&transaction, draft.message_id, &draft.recipients)?;
    if previous_body.as_deref() != Some(draft.body_file_key.as_str())
        && let Some(previous_body) = previous_body
    {
        queue_if_unreferenced(&transaction, &previous_body, draft.now_ms)?;
    }
    let snapshot = load_draft_from(&transaction, draft.message_id)?
        .ok_or_else(|| DbFailure::database("updated draft could not be loaded"))?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(snapshot)
}

pub(crate) fn load_draft(
    connection: &Connection,
    message_id: MessageId,
) -> Result<Option<DraftSnapshot>, DbFailure> {
    load_draft_from(connection, message_id)
}

pub(crate) fn load_latest_draft(
    connection: &Connection,
    account_id: AccountId,
) -> Result<Option<DraftSnapshot>, DbFailure> {
    let message_id = connection
        .query_row(
            "SELECT local.message_id
             FROM local_drafts AS local
             JOIN messages AS message ON message.id = local.message_id
             WHERE message.account_id = ?1
             ORDER BY local.updated_at_ms DESC, local.message_id DESC
             LIMIT 1",
            [account_id.get()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(DbFailure::database)?;
    message_id
        .map(|id| load_draft_from(connection, MessageId::from_database(id)))
        .transpose()
        .map(Option::flatten)
}

fn load_draft_from(
    connection: &Connection,
    message_id: MessageId,
) -> Result<Option<DraftSnapshot>, DbFailure> {
    type StoredDraft = (
        i64,
        i64,
        String,
        String,
        String,
        String,
        i64,
        i64,
        Option<i64>,
    );
    let stored: Option<StoredDraft> = connection
        .query_row(
            "SELECT message.account_id, message.revision, message.subject, message.preview,
                    content.reader_excerpt, content.body_file_key, content.body_byte_count,
                    local.updated_at_ms, local.locked_artifact_generation
             FROM messages AS message
             JOIN local_drafts AS local ON local.message_id = message.id
             JOIN message_content AS content ON content.message_id = message.id
             WHERE message.id = ?1",
            [message_id.get()],
            |row| {
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
                ))
            },
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((
        account_id,
        revision,
        subject,
        preview,
        reader_excerpt,
        body_key,
        body_bytes,
        updated_at_ms,
        lock,
    )) = stored
    else {
        return Ok(None);
    };
    let body_file_key = FileKey::parse(&body_key)
        .map_err(|_| DbFailure::database("draft contains an invalid body file key"))?;
    if !body_file_key.as_str().starts_with("body/") {
        return Err(DbFailure::database(
            "draft body does not use a body file key",
        ));
    }
    let mut statement = connection
        .prepare(
            "SELECT address, display_name
             FROM draft_recipients
             WHERE message_id = ?1
             ORDER BY ordinal
             LIMIT 65",
        )
        .map_err(DbFailure::database)?;
    let recipients = statement
        .query_map([message_id.get()], |row| {
            Ok(DraftRecipient {
                address: row.get::<_, String>(0)?.into_boxed_str(),
                display_name: row.get::<_, String>(1)?.into_boxed_str(),
            })
        })
        .map_err(DbFailure::database)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(DbFailure::database)?;
    if recipients.len() > MAX_DRAFT_RECIPIENTS {
        return Err(DbFailure::resource_limit(
            "stored draft recipient limit exceeded",
        ));
    }
    Ok(Some(DraftSnapshot {
        message_id,
        account_id: AccountId::new(account_id)
            .map_err(|error| DbFailure::database(error.to_string()))?,
        revision: u64::try_from(revision)
            .map_err(|_| DbFailure::database("invalid draft revision"))?,
        subject: subject.into_boxed_str(),
        preview: preview.into_boxed_str(),
        reader_excerpt: reader_excerpt.into_boxed_str(),
        body_file_key,
        body_byte_count: u64::try_from(body_bytes)
            .map_err(|_| DbFailure::database("invalid draft body byte count"))?,
        recipients: recipients.into_boxed_slice(),
        updated_at_ms,
        locked_artifact_generation: lock
            .map(u64::try_from)
            .transpose()
            .map_err(|_| DbFailure::database("invalid draft artifact lock"))?,
    }))
}

fn require_account_fence(
    connection: &Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<String, DbFailure> {
    let stored = connection
        .query_row(
            "SELECT address, configuration_generation, state
             FROM accounts WHERE id = ?1",
            [account_id.get()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((address, generation, state)) = stored else {
        return Err(DbFailure::not_found("draft account no longer exists"));
    };
    if generation != expected_generation.get() {
        return Err(DbFailure::conflict("draft account generation changed"));
    }
    let lifecycle = match state.as_str() {
        "active" => AccountLifecycle::Active,
        "disabled" => AccountLifecycle::Disabled,
        "removing_credentials" => AccountLifecycle::RemovingCredentials,
        "removing_cache" => AccountLifecycle::RemovingCache,
        _ => return Err(DbFailure::database("invalid draft account lifecycle")),
    };
    if lifecycle != AccountLifecycle::Active {
        return Err(DbFailure::conflict("draft account is not active"));
    }
    Ok(address)
}

fn ensure_drafts_folder(
    transaction: &Transaction<'_>,
    account_id: AccountId,
) -> Result<i64, DbFailure> {
    if let Some(folder_id) = transaction
        .query_row(
            "SELECT id FROM folders WHERE account_id = ?1 AND role = 'drafts' LIMIT 1",
            [account_id.get()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(DbFailure::database)?
    {
        return Ok(folder_id);
    }
    transaction
        .execute(
            "INSERT INTO folders (account_id, remote_key, name, role)
             VALUES (?1, 'local:drafts', 'Drafts', 'drafts')",
            [account_id.get()],
        )
        .map_err(map_write_error)?;
    Ok(transaction.last_insert_rowid())
}

fn replace_recipients(
    transaction: &Transaction<'_>,
    message_id: MessageId,
    recipients: &[DraftRecipient],
) -> Result<(), DbFailure> {
    transaction
        .execute(
            "DELETE FROM draft_recipients WHERE message_id = ?1",
            [message_id.get()],
        )
        .map_err(map_write_error)?;
    for (ordinal, recipient) in recipients.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO draft_recipients
                     (message_id, ordinal, address, display_name)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    message_id.get(),
                    ordinal as i64,
                    recipient.address.as_ref(),
                    recipient.display_name.as_ref(),
                ],
            )
            .map_err(map_write_error)?;
    }
    Ok(())
}

fn queue_if_unreferenced(
    transaction: &Transaction<'_>,
    file_key: &str,
    now_ms: i64,
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
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM file_staging WHERE file_key = ?1
                   )",
            params![file_key, now_ms],
        )
        .map_err(map_write_error)?;
    Ok(())
}

fn record_draft_created(
    transaction: &Transaction<'_>,
    account_id: AccountId,
) -> Result<(), DbFailure> {
    let updated = transaction
        .execute(
            "UPDATE account_mailbox_stats
             SET drafts_total = drafts_total + 1, dirty = 0
             WHERE account_id = ?1 AND dirty = 1
               AND drafts_total < 9223372036854775807",
            [account_id.get()],
        )
        .map_err(map_write_error)?;
    if updated == 1 {
        Ok(())
    } else {
        Err(DbFailure::conflict(
            "mailbox statistics are missing or inconsistent",
        ))
    }
}

fn require_clean_stats(
    transaction: &Transaction<'_>,
    account_id: AccountId,
) -> Result<(), DbFailure> {
    let dirty = transaction
        .query_row(
            "SELECT dirty FROM account_mailbox_stats WHERE account_id = ?1",
            [account_id.get()],
            |row| row.get::<_, bool>(0),
        )
        .optional()
        .map_err(DbFailure::database)?;
    match dirty {
        Some(false) => Ok(()),
        Some(true) => Err(DbFailure::conflict(
            "mailbox statistics must be rebuilt before creating a draft",
        )),
        None => Err(DbFailure::conflict(
            "mail account is missing its statistics row",
        )),
    }
}

fn validate_body(file_key: &FileKey, byte_count: u64) -> Result<(), DbFailure> {
    if !file_key.as_str().starts_with("body/") {
        return Err(DbFailure::conflict("draft body must use a body file key"));
    }
    if byte_count > MAX_DRAFT_BODY_BYTES {
        return Err(DbFailure::resource_limit(format!(
            "draft body exceeds the {MAX_DRAFT_BODY_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn validate_recipients(recipients: &[DraftRecipient]) -> Result<(), DbFailure> {
    if recipients.len() > MAX_DRAFT_RECIPIENTS {
        return Err(DbFailure::resource_limit(format!(
            "draft exceeds the {MAX_DRAFT_RECIPIENTS}-recipient limit"
        )));
    }
    Ok(())
}

fn validate_text(
    value: &str,
    maximum: usize,
    allow_empty: bool,
    label: &str,
) -> Result<(), DbFailure> {
    if (!allow_empty && value.is_empty()) || value.len() > maximum {
        return Err(DbFailure::resource_limit(format!(
            "{label} exceeds its {maximum}-byte limit or is empty"
        )));
    }
    Ok(())
}

fn validate_timestamp(value: i64) -> Result<(), DbFailure> {
    if (MIN_TIMESTAMP_MS..=MAX_TIMESTAMP_MS).contains(&value) {
        Ok(())
    } else {
        Err(DbFailure::resource_limit(
            "draft timestamp is outside SQLite bounds",
        ))
    }
}

fn map_write_error(error: rusqlite::Error) -> DbFailure {
    if matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ConstraintViolation)
    ) {
        DbFailure::resource_limit(error.to_string())
    } else {
        DbFailure::database(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sqlite::{domain::FailureKind, migrations::migrate};

    fn database() -> Connection {
        let mut connection = Connection::open_in_memory().expect("open draft test database");
        migrate(&mut connection).expect("migrate draft test database");
        connection
            .execute(
                "INSERT INTO accounts
                     (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (1, 'imap', 'account-1', 'Account', 'sender@example.test', 'active', 0)",
                [],
            )
            .expect("insert draft account");
        connection
            .execute(
                "INSERT INTO account_connections
                     (account_id, credential_key, auth_kind, login_name, imap_host, imap_port,
                      smtp_host, smtp_port, smtp_security, smtp_state)
                 VALUES (1, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'app_password',
                         'sender@example.test', 'imap.example.test', 993,
                         'smtp.example.test', 465, 'implicit_tls', 'configured')",
                [],
            )
            .expect("insert draft connection");
        connection
    }

    fn body(value: u8) -> FileKey {
        FileKey::parse(&format!("body/{value:032x}.txt")).expect("valid body key")
    }

    fn recipient(value: usize) -> DraftRecipient {
        DraftRecipient::new(&format!("recipient-{value}@example.test"), "Recipient")
            .expect("valid draft recipient")
    }

    #[test]
    fn persists_reopens_and_revision_fences_bounded_drafts() {
        let mut connection = database();
        let account_id = AccountId::new(1).unwrap();
        let generation = AccountGeneration::new(1).unwrap();
        let created = create_draft(
            &mut connection,
            &NewDraft::new(
                account_id,
                generation,
                "local:draft-1",
                "Subject",
                "Preview",
                "Full reader excerpt",
                body(1),
                19,
                vec![recipient(1)],
                1_000,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(created.revision, 0);
        assert_eq!(created.preview.as_ref(), "Preview");
        assert_eq!(created.reader_excerpt.as_ref(), "Full reader excerpt");
        assert_eq!(created.recipients.len(), 1);
        let created_stats: (i64, bool) = connection
            .query_row(
                "SELECT drafts_total, dirty FROM account_mailbox_stats WHERE account_id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(created_stats, (1, false));
        assert_eq!(
            load_draft(&connection, created.message_id).unwrap(),
            Some(created.clone())
        );
        assert_eq!(
            load_latest_draft(&connection, account_id)
                .unwrap()
                .map(|draft| draft.message_id),
            Some(created.message_id)
        );

        let stale = DraftUpdate::new(
            created.message_id,
            account_id,
            generation,
            1,
            "Stale",
            "Stale",
            "Stale",
            body(2),
            5,
            vec![],
            2_000,
        )
        .unwrap();
        assert_eq!(
            update_draft(&mut connection, &stale).unwrap_err().kind,
            FailureKind::Conflict
        );

        let updated = update_draft(
            &mut connection,
            &DraftUpdate::new(
                created.message_id,
                account_id,
                generation,
                0,
                "Updated",
                "Updated preview",
                "Updated excerpt",
                body(3),
                15,
                vec![recipient(2), recipient(3)],
                3_000,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(updated.revision, 1);
        assert_eq!(updated.subject.as_ref(), "Updated");
        assert_eq!(updated.recipients.len(), 2);
        let updated_stats: (i64, bool) = connection
            .query_row(
                "SELECT drafts_total, dirty FROM account_mailbox_stats WHERE account_id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(updated_stats, (1, false));
        let queued_old_body: i64 = connection
            .query_row(
                "SELECT count(*) FROM file_gc WHERE file_key = ?1",
                [body(1).as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(queued_old_body, 1);

        let too_many = (0..=MAX_DRAFT_RECIPIENTS)
            .map(recipient)
            .collect::<Vec<_>>();
        assert_eq!(
            NewDraft::new(
                account_id,
                generation,
                "local:too-many",
                "",
                "",
                "",
                body(4),
                0,
                too_many,
                4_000,
            )
            .unwrap_err()
            .kind,
            FailureKind::ResourceLimit
        );
        assert_eq!(
            NewDraft::new(
                account_id,
                generation,
                "local:too-large",
                "",
                "",
                "",
                body(5),
                MAX_DRAFT_BODY_BYTES + 1,
                vec![],
                4_000,
            )
            .unwrap_err()
            .kind,
            FailureKind::ResourceLimit
        );
    }
}
