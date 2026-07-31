use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::content::FileKey;

use super::{
    account::{AccountGeneration, SmtpSecurity},
    domain::{AccountId, DbFailure, MessageId},
    stats::{apply_transition, load_message_snapshot},
};

pub(crate) const MAX_OUTBOUND_MIME_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RECIPIENTS: usize = 64;
const MAX_ADDRESS_BYTES: usize = 320;
const MAX_DISPLAY_NAME_BYTES: usize = 320;
const MAX_MESSAGE_ID_BYTES: usize = 998;
const MAX_ERROR_CODE_BYTES: usize = 64;
pub(crate) const MAX_OUTBOX_SUMMARIES: u8 = 64;
const MAX_CLAIM_SCAN: usize = 32;
const MAX_DELIVERY_ATTEMPTS: i64 = 1_000;
const MIN_TIMESTAMP_MS: i64 = -62_135_596_800_000;
const MAX_TIMESTAMP_MS: i64 = 253_402_300_799_999;
const MAX_RESERVATION_TTL_MS: i64 = 15 * 60 * 1_000;
const CLAIM_TTL_MS: i64 = 5 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutboxState {
    Reserved,
    Ready,
    InFlight,
    RetryWait,
    Uncertain,
    PermanentFailure,
    Delivered,
}

impl OutboxState {
    fn database_value(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Ready => "ready",
            Self::InFlight => "in_flight",
            Self::RetryWait => "retry_wait",
            Self::Uncertain => "uncertain",
            Self::PermanentFailure => "permanent_failure",
            Self::Delivered => "delivered",
        }
    }

    fn from_database(value: &str) -> Result<Self, DbFailure> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "ready" => Ok(Self::Ready),
            "in_flight" => Ok(Self::InFlight),
            "retry_wait" => Ok(Self::RetryWait),
            "uncertain" => Ok(Self::Uncertain),
            "permanent_failure" => Ok(Self::PermanentFailure),
            "delivered" => Ok(Self::Delivered),
            _ => Err(DbFailure::database("invalid outbox state")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecipientKind {
    To,
    Cc,
    Bcc,
}

impl RecipientKind {
    fn database_value(self) -> &'static str {
        match self {
            Self::To => "to",
            Self::Cc => "cc",
            Self::Bcc => "bcc",
        }
    }

    fn from_database(value: &str) -> Result<Self, DbFailure> {
        match value {
            "to" => Ok(Self::To),
            "cc" => Ok(Self::Cc),
            "bcc" => Ok(Self::Bcc),
            _ => Err(DbFailure::database("invalid outbox recipient kind")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutboxRecipient {
    pub(crate) kind: RecipientKind,
    pub(crate) address: Box<str>,
    pub(crate) display_name: Box<str>,
}

impl OutboxRecipient {
    pub(crate) fn new(
        kind: RecipientKind,
        address: &str,
        display_name: &str,
    ) -> Result<Self, DbFailure> {
        validate_text(
            address,
            MAX_ADDRESS_BYTES,
            false,
            "outbox recipient address",
        )?;
        validate_text(
            display_name,
            MAX_DISPLAY_NAME_BYTES,
            true,
            "outbox recipient name",
        )?;
        Ok(Self {
            kind,
            address: address.into(),
            display_name: display_name.into(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct OutboxReservationToken([u8; 16]);

impl OutboxReservationToken {
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

    fn decode(value: &str) -> Result<Self, DbFailure> {
        if value.len() != 32 {
            return Err(DbFailure::database("invalid outbox reservation token"));
        }
        let mut bytes = [0_u8; 16];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (decode_hex(pair[0])? << 4) | decode_hex(pair[1])?;
        }
        Ok(Self(bytes))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutboxReserveRequest {
    message_id: MessageId,
    account_id: AccountId,
    expected_generation: AccountGeneration,
    expected_draft_revision: u64,
    token: OutboxReservationToken,
    rfc_message_id: Box<str>,
    recipients: Box<[OutboxRecipient]>,
    created_at_ms: i64,
    expires_at_ms: i64,
}

impl OutboxReserveRequest {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        message_id: MessageId,
        account_id: AccountId,
        expected_generation: AccountGeneration,
        expected_draft_revision: u64,
        token: OutboxReservationToken,
        rfc_message_id: &str,
        recipients: Vec<OutboxRecipient>,
        created_at_ms: i64,
        expires_at_ms: i64,
    ) -> Result<Self, DbFailure> {
        if expected_draft_revision > i64::MAX as u64 {
            return Err(DbFailure::resource_limit(
                "outbox draft revision is outside SQLite bounds",
            ));
        }
        validate_text(
            rfc_message_id,
            MAX_MESSAGE_ID_BYTES,
            false,
            "RFC message id",
        )?;
        validate_recipients(&recipients)?;
        validate_timestamp(created_at_ms)?;
        validate_timestamp(expires_at_ms)?;
        let ttl = expires_at_ms
            .checked_sub(created_at_ms)
            .ok_or_else(|| DbFailure::resource_limit("outbox reservation TTL overflow"))?;
        if !(1..=MAX_RESERVATION_TTL_MS).contains(&ttl) {
            return Err(DbFailure::resource_limit(
                "outbox reservation TTL must be positive and at most 15 minutes",
            ));
        }
        Ok(Self {
            message_id,
            account_id,
            expected_generation,
            expected_draft_revision,
            token,
            rfc_message_id: rfc_message_id.into(),
            recipients: recipients.into_boxed_slice(),
            created_at_ms,
            expires_at_ms,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutboxReservation {
    pub(crate) message_id: MessageId,
    pub(crate) account_id: AccountId,
    pub(crate) configuration_generation: AccountGeneration,
    pub(crate) draft_revision: u64,
    pub(crate) artifact_generation: u64,
    pub(crate) token: OutboxReservationToken,
    pub(crate) file_key: FileKey,
    pub(crate) rfc_message_id: Box<str>,
    pub(crate) expires_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArtifactObservation {
    Missing,
    Published { byte_count: u64 },
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReservationRecovery {
    Rebuild(OutboxReservation),
    Ready,
    PermanentFailure,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutboxLease {
    pub(crate) message_id: MessageId,
    pub(crate) artifact_generation: u64,
    pub(crate) claim_epoch: u64,
    pub(crate) configuration_generation: AccountGeneration,
    pub(crate) expires_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutboxClaim {
    pub(crate) lease: OutboxLease,
    pub(crate) account_id: AccountId,
    pub(crate) credential_key: Box<str>,
    pub(crate) login_name: Box<str>,
    pub(crate) smtp_host: Box<str>,
    pub(crate) smtp_port: u16,
    pub(crate) smtp_security: SmtpSecurity,
    pub(crate) envelope_from: Box<str>,
    pub(crate) rfc_message_id: Box<str>,
    pub(crate) file_key: FileKey,
    pub(crate) wire_byte_count: u64,
    pub(crate) recipients: Box<[OutboxRecipient]>,
    pub(crate) attempt_count: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OutboxClaimOutcome {
    Claimed(Box<OutboxClaim>),
    Idle { wake_at_ms: Option<i64> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutboxErrorClass {
    Network,
    RateLimit,
    Authentication,
    Configuration,
    Protocol,
    Permanent,
    Ambiguous,
}

impl OutboxErrorClass {
    fn database_value(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::RateLimit => "rate_limit",
            Self::Authentication => "authentication",
            Self::Configuration => "configuration",
            Self::Protocol => "protocol",
            Self::Permanent => "permanent",
            Self::Ambiguous => "ambiguous",
        }
    }

    fn from_database(value: &str) -> Result<Self, DbFailure> {
        match value {
            "network" => Ok(Self::Network),
            "rate_limit" => Ok(Self::RateLimit),
            "authentication" => Ok(Self::Authentication),
            "configuration" => Ok(Self::Configuration),
            "protocol" => Ok(Self::Protocol),
            "permanent" => Ok(Self::Permanent),
            "ambiguous" => Ok(Self::Ambiguous),
            _ => Err(DbFailure::database("invalid outbox error class")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutboxActionFence {
    pub(crate) message_id: MessageId,
    pub(crate) account_id: AccountId,
    pub(crate) configuration_generation: AccountGeneration,
    pub(crate) artifact_generation: u64,
}

impl OutboxActionFence {
    pub(crate) fn new(
        message_id: MessageId,
        account_id: AccountId,
        configuration_generation: AccountGeneration,
        artifact_generation: u64,
    ) -> Result<Self, DbFailure> {
        validate_artifact_generation(artifact_generation)?;
        Ok(Self {
            message_id,
            account_id,
            configuration_generation,
            artifact_generation,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutboxRecipientSummary {
    pub(crate) first_to_address: Option<Box<str>>,
    pub(crate) to_count: u32,
    pub(crate) cc_count: u32,
    pub(crate) bcc_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutboxSummary {
    pub(crate) message_id: MessageId,
    pub(crate) account_id: AccountId,
    pub(crate) configuration_generation: AccountGeneration,
    pub(crate) artifact_generation: u64,
    pub(crate) state: OutboxState,
    pub(crate) subject: Box<str>,
    pub(crate) recipients: OutboxRecipientSummary,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    pub(crate) not_before_ms: Option<i64>,
    pub(crate) attempt_count: u16,
    pub(crate) error_class: Option<OutboxErrorClass>,
    pub(crate) error_code: Option<Box<str>>,
}

impl OutboxSummary {
    pub(crate) fn action_fence(&self) -> OutboxActionFence {
        OutboxActionFence {
            message_id: self.message_id,
            account_id: self.account_id,
            configuration_generation: self.configuration_generation,
            artifact_generation: self.artifact_generation,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutboxSummaryPage {
    pub(crate) items: Box<[OutboxSummary]>,
    pub(crate) has_more: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UncertainResolution {
    AssumeDelivered,
    Release,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OutboxReport {
    Delivered {
        delivered_at_ms: i64,
    },
    Retry {
        not_before_ms: i64,
        error_class: OutboxErrorClass,
        error_code: Box<str>,
    },
    RetryAfterRejection {
        not_before_ms: i64,
        error_class: OutboxErrorClass,
        error_code: Box<str>,
    },
    PermanentFailure {
        error_class: OutboxErrorClass,
        error_code: Box<str>,
    },
    Uncertain {
        error_code: Box<str>,
    },
}

impl OutboxReport {
    pub(crate) fn delivered(delivered_at_ms: i64) -> Result<Self, DbFailure> {
        validate_timestamp(delivered_at_ms)?;
        Ok(Self::Delivered { delivered_at_ms })
    }

    pub(crate) fn retry(
        not_before_ms: i64,
        error_class: OutboxErrorClass,
        error_code: &str,
    ) -> Result<Self, DbFailure> {
        validate_timestamp(not_before_ms)?;
        validate_error_code(error_code)?;
        Ok(Self::Retry {
            not_before_ms,
            error_class,
            error_code: error_code.into(),
        })
    }

    pub(crate) fn retry_after_rejection(
        not_before_ms: i64,
        error_class: OutboxErrorClass,
        error_code: &str,
    ) -> Result<Self, DbFailure> {
        validate_timestamp(not_before_ms)?;
        validate_error_code(error_code)?;
        Ok(Self::RetryAfterRejection {
            not_before_ms,
            error_class,
            error_code: error_code.into(),
        })
    }

    pub(crate) fn permanent_failure(
        error_class: OutboxErrorClass,
        error_code: &str,
    ) -> Result<Self, DbFailure> {
        validate_error_code(error_code)?;
        Ok(Self::PermanentFailure {
            error_class,
            error_code: error_code.into(),
        })
    }

    pub(crate) fn uncertain(error_code: &str) -> Result<Self, DbFailure> {
        validate_error_code(error_code)?;
        Ok(Self::Uncertain {
            error_code: error_code.into(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutboxReportOutcome {
    Applied(OutboxState),
    Stale,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OutboxRecoveryOutcome {
    Reservation(OutboxReservation),
    Recovered {
        message_id: MessageId,
        state: OutboxState,
    },
    Idle,
}

pub(crate) fn load_outbox_summaries(
    connection: &Connection,
    limit: u8,
) -> Result<OutboxSummaryPage, DbFailure> {
    if !(1..=MAX_OUTBOX_SUMMARIES).contains(&limit) {
        return Err(DbFailure::resource_limit(format!(
            "outbox summary limit must be between 1 and {MAX_OUTBOX_SUMMARIES}"
        )));
    }
    let query_limit = i64::from(limit) + 1;
    let mut statement = connection
        .prepare(
            "SELECT outbox.message_id, outbox.account_id,
                    outbox.configuration_generation, outbox.artifact_generation,
                    outbox.state, message.subject,
                    (SELECT recipient.address FROM outbox_recipients AS recipient
                     WHERE recipient.message_id = outbox.message_id AND recipient.kind = 'to'
                     ORDER BY recipient.ordinal LIMIT 1),
                    (SELECT count(*) FROM outbox_recipients AS recipient
                     WHERE recipient.message_id = outbox.message_id AND recipient.kind = 'to'),
                    (SELECT count(*) FROM outbox_recipients AS recipient
                     WHERE recipient.message_id = outbox.message_id AND recipient.kind = 'cc'),
                    (SELECT count(*) FROM outbox_recipients AS recipient
                     WHERE recipient.message_id = outbox.message_id AND recipient.kind = 'bcc'),
                    outbox.created_at_ms, outbox.updated_at_ms, outbox.not_before_ms,
                    outbox.attempt_count, outbox.error_class, outbox.error_code
             FROM outbox
             JOIN messages AS message ON message.id = outbox.message_id
             WHERE outbox.state <> 'delivered'
             ORDER BY outbox.updated_at_ms DESC, outbox.message_id DESC
             LIMIT ?1",
        )
        .map_err(DbFailure::database)?;
    let mut rows = statement
        .query([query_limit])
        .map_err(DbFailure::database)?;
    let mut items = Vec::with_capacity(usize::from(limit) + 1);
    while let Some(row) = rows.next().map_err(DbFailure::database)? {
        let raw_message_id = row.get::<_, i64>(0).map_err(DbFailure::database)?;
        let raw_account_id = row.get::<_, i64>(1).map_err(DbFailure::database)?;
        let raw_configuration_generation = row.get::<_, i64>(2).map_err(DbFailure::database)?;
        let raw_artifact_generation = row.get::<_, i64>(3).map_err(DbFailure::database)?;
        let raw_state = row.get::<_, String>(4).map_err(DbFailure::database)?;
        let subject = row.get::<_, String>(5).map_err(DbFailure::database)?;
        let first_to_address = row
            .get::<_, Option<String>>(6)
            .map_err(DbFailure::database)?;
        let to_count = row.get::<_, i64>(7).map_err(DbFailure::database)?;
        let cc_count = row.get::<_, i64>(8).map_err(DbFailure::database)?;
        let bcc_count = row.get::<_, i64>(9).map_err(DbFailure::database)?;
        let created_at_ms = row.get::<_, i64>(10).map_err(DbFailure::database)?;
        let updated_at_ms = row.get::<_, i64>(11).map_err(DbFailure::database)?;
        let not_before_ms = row.get::<_, Option<i64>>(12).map_err(DbFailure::database)?;
        let raw_attempt_count = row.get::<_, i64>(13).map_err(DbFailure::database)?;
        let raw_error_class = row
            .get::<_, Option<String>>(14)
            .map_err(DbFailure::database)?;
        let error_code = row
            .get::<_, Option<String>>(15)
            .map_err(DbFailure::database)?;

        items.push(OutboxSummary {
            message_id: MessageId::new(raw_message_id)
                .map_err(|error| DbFailure::database(error.to_string()))?,
            account_id: AccountId::new(raw_account_id)
                .map_err(|error| DbFailure::database(error.to_string()))?,
            configuration_generation: AccountGeneration::new(raw_configuration_generation)
                .map_err(|error| DbFailure::database(error.to_string()))?,
            artifact_generation: u64::try_from(raw_artifact_generation)
                .ok()
                .filter(|generation| *generation > 0)
                .ok_or_else(|| DbFailure::database("invalid outbox artifact generation"))?,
            state: OutboxState::from_database(&raw_state)?,
            subject: subject.into_boxed_str(),
            recipients: OutboxRecipientSummary {
                first_to_address: first_to_address.map(String::into_boxed_str),
                to_count: stored_recipient_count(to_count)?,
                cc_count: stored_recipient_count(cc_count)?,
                bcc_count: stored_recipient_count(bcc_count)?,
            },
            created_at_ms,
            updated_at_ms,
            not_before_ms,
            attempt_count: u16::try_from(raw_attempt_count)
                .map_err(|_| DbFailure::database("invalid outbox attempt count"))?,
            error_class: raw_error_class
                .as_deref()
                .map(OutboxErrorClass::from_database)
                .transpose()?,
            error_code: error_code.map(String::into_boxed_str),
        });
    }
    let has_more = items.len() > usize::from(limit);
    items.truncate(usize::from(limit));
    Ok(OutboxSummaryPage {
        items: items.into_boxed_slice(),
        has_more,
    })
}

pub(crate) fn reserve_outbox(
    connection: &mut Connection,
    request: &OutboxReserveRequest,
) -> Result<OutboxReservation, DbFailure> {
    let transaction = immediate_transaction(connection)?;
    let account: Option<(i64, String, String)> = transaction
        .query_row(
            "SELECT account.configuration_generation, account.address, connection.smtp_state
             FROM accounts AS account
             JOIN account_connections AS connection ON connection.account_id = account.id
             WHERE account.id = ?1 AND account.state = 'active'",
            [request.account_id.get()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((generation, envelope_from, smtp_state)) = account else {
        return Err(DbFailure::not_found(
            "active outbox account configuration no longer exists",
        ));
    };
    if generation != request.expected_generation.get() {
        return Err(DbFailure::conflict("outbox account generation changed"));
    }
    if smtp_state != "configured" {
        return Err(DbFailure::conflict(
            "SMTP endpoint must be configured explicitly before sending",
        ));
    }
    let draft: Option<(i64, Option<i64>)> = transaction
        .query_row(
            "SELECT message.revision, local.locked_artifact_generation
             FROM messages AS message
             JOIN local_drafts AS local ON local.message_id = message.id
             WHERE message.id = ?1 AND message.account_id = ?2",
            params![request.message_id.get(), request.account_id.get()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((revision, lock)) = draft else {
        return Err(DbFailure::not_found("outbox draft no longer exists"));
    };
    if revision != request.expected_draft_revision as i64 {
        return Err(DbFailure::conflict("outbox draft revision changed"));
    }
    if lock.is_some() {
        return Err(DbFailure::conflict(
            "draft already has an outbound reservation",
        ));
    }
    if request.recipients.is_empty()
        || !request
            .recipients
            .iter()
            .any(|row| row.kind == RecipientKind::To)
    {
        return Err(DbFailure::conflict(
            "outbox requires at least one To recipient",
        ));
    }
    let artifact_generation = 1_i64;
    let encoded = request.token.encoded();
    let file_key = FileKey::parse(&format!("outbound/{encoded}.eml"))
        .map_err(|_| DbFailure::database("could not create outbound MIME file key"))?;
    transaction
        .execute(
            "INSERT INTO outbox
                 (message_id, account_id, configuration_generation, artifact_generation,
                  draft_revision, reservation_token, reservation_expires_at_ms,
                  mime_file_key, rfc_message_id, envelope_from, state,
                  created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                     'reserved', ?11, ?11)",
            params![
                request.message_id.get(),
                request.account_id.get(),
                generation,
                artifact_generation,
                revision,
                encoded,
                request.expires_at_ms,
                file_key.as_str(),
                request.rfc_message_id.as_ref(),
                envelope_from,
                request.created_at_ms,
            ],
        )
        .map_err(map_write_error)?;
    insert_recipients(&transaction, request.message_id, &request.recipients)?;
    let locked = transaction
        .execute(
            "UPDATE local_drafts
             SET locked_artifact_generation = ?2
             WHERE message_id = ?1 AND locked_artifact_generation IS NULL",
            params![request.message_id.get(), artifact_generation],
        )
        .map_err(map_write_error)?;
    if locked != 1 {
        return Err(DbFailure::conflict("draft reservation lock changed"));
    }
    let reservation = OutboxReservation {
        message_id: request.message_id,
        account_id: request.account_id,
        configuration_generation: request.expected_generation,
        draft_revision: request.expected_draft_revision,
        artifact_generation: artifact_generation as u64,
        token: request.token,
        file_key,
        rfc_message_id: request.rfc_message_id.clone(),
        expires_at_ms: request.expires_at_ms,
    };
    transaction.commit().map_err(DbFailure::database)?;
    Ok(reservation)
}

pub(crate) fn finalize_outbox(
    connection: &mut Connection,
    reservation: &OutboxReservation,
    wire_byte_count: u64,
    now_ms: i64,
) -> Result<OutboxReportOutcome, DbFailure> {
    validate_wire_bytes(wire_byte_count)?;
    validate_timestamp(now_ms)?;
    let transaction = immediate_transaction(connection)?;
    let updated = transaction
        .execute(
            "UPDATE outbox
             SET state = 'ready', reservation_token = NULL,
                 reservation_expires_at_ms = NULL, wire_byte_count = ?7,
                 not_before_ms = ?8, updated_at_ms = ?8,
                 error_class = NULL, error_code = NULL
             WHERE message_id = ?1 AND account_id = ?2
               AND configuration_generation = ?3 AND artifact_generation = ?4
               AND draft_revision = ?5 AND reservation_token = ?6
               AND state = 'reserved'
               AND EXISTS (
                   SELECT 1 FROM accounts
                   WHERE id = ?2 AND configuration_generation = ?3 AND state = 'active'
               )
               AND EXISTS (
                   SELECT 1 FROM local_drafts
                   WHERE message_id = ?1 AND locked_artifact_generation = ?4
               )",
            params![
                reservation.message_id.get(),
                reservation.account_id.get(),
                reservation.configuration_generation.get(),
                reservation.artifact_generation as i64,
                reservation.draft_revision as i64,
                reservation.token.encoded(),
                wire_byte_count as i64,
                now_ms,
            ],
        )
        .map_err(map_write_error)?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(if updated == 1 {
        OutboxReportOutcome::Applied(OutboxState::Ready)
    } else {
        OutboxReportOutcome::Stale
    })
}

pub(crate) fn recover_reservation(
    connection: &mut Connection,
    reservation: &OutboxReservation,
    observation: ArtifactObservation,
    now_ms: i64,
) -> Result<ReservationRecovery, DbFailure> {
    validate_timestamp(now_ms)?;
    match observation {
        ArtifactObservation::Published { byte_count } => Ok(
            match finalize_outbox(connection, reservation, byte_count, now_ms)? {
                OutboxReportOutcome::Applied(_) => ReservationRecovery::Ready,
                OutboxReportOutcome::Stale => ReservationRecovery::Stale,
            },
        ),
        ArtifactObservation::Missing => {
            let expires_at_ms = now_ms
                .checked_add(MAX_RESERVATION_TTL_MS)
                .ok_or_else(|| DbFailure::resource_limit("reservation recovery TTL overflow"))?;
            validate_timestamp(expires_at_ms)?;
            let updated = connection
                .execute(
                    "UPDATE outbox
                     SET reservation_expires_at_ms = ?7, updated_at_ms = ?6
                     WHERE message_id = ?1 AND account_id = ?2
                       AND configuration_generation = ?3 AND artifact_generation = ?4
                       AND reservation_token = ?5 AND state = 'reserved'",
                    params![
                        reservation.message_id.get(),
                        reservation.account_id.get(),
                        reservation.configuration_generation.get(),
                        reservation.artifact_generation as i64,
                        reservation.token.encoded(),
                        now_ms,
                        expires_at_ms,
                    ],
                )
                .map_err(map_write_error)?;
            if updated == 0 {
                return Ok(ReservationRecovery::Stale);
            }
            let mut renewed = reservation.clone();
            renewed.expires_at_ms = expires_at_ms;
            Ok(ReservationRecovery::Rebuild(renewed))
        }
        ArtifactObservation::Invalid => {
            let updated = connection
                .execute(
                    "UPDATE outbox
                     SET state = 'permanent_failure', reservation_token = NULL,
                         reservation_expires_at_ms = NULL, error_class = 'permanent',
                         error_code = 'invalid_artifact', updated_at_ms = ?6
                     WHERE message_id = ?1 AND account_id = ?2
                       AND configuration_generation = ?3 AND artifact_generation = ?4
                       AND reservation_token = ?5 AND state = 'reserved'",
                    params![
                        reservation.message_id.get(),
                        reservation.account_id.get(),
                        reservation.configuration_generation.get(),
                        reservation.artifact_generation as i64,
                        reservation.token.encoded(),
                        now_ms,
                    ],
                )
                .map_err(map_write_error)?;
            Ok(if updated == 1 {
                ReservationRecovery::PermanentFailure
            } else {
                ReservationRecovery::Stale
            })
        }
    }
}

pub(crate) fn recover_outbox(
    connection: &mut Connection,
    now_ms: i64,
) -> Result<OutboxRecoveryOutcome, DbFailure> {
    validate_timestamp(now_ms)?;
    let transaction = immediate_transaction(connection)?;
    if let Some(reservation) = load_expired_reservation(&transaction, now_ms)? {
        transaction.commit().map_err(DbFailure::database)?;
        return Ok(OutboxRecoveryOutcome::Reservation(reservation));
    }
    let expired: Option<(i64, i64)> = transaction
        .query_row(
            "SELECT message_id, delivery_started
             FROM outbox
             WHERE state = 'in_flight' AND lease_expires_at_ms <= ?1
             ORDER BY lease_expires_at_ms, message_id
             LIMIT 1",
            [now_ms],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((raw_message_id, delivery_started)) = expired else {
        transaction.commit().map_err(DbFailure::database)?;
        return Ok(OutboxRecoveryOutcome::Idle);
    };
    let message_id = MessageId::from_database(raw_message_id);
    let state = if delivery_started == 0 {
        transaction
            .execute(
                "UPDATE outbox
                 SET state = 'ready', lease_expires_at_ms = NULL,
                     delivery_started = 0, not_before_ms = ?2,
                     error_class = 'network', error_code = 'lease_expired_before_data',
                     updated_at_ms = ?2
                 WHERE message_id = ?1 AND state = 'in_flight'",
                params![raw_message_id, now_ms],
            )
            .map_err(map_write_error)?;
        OutboxState::Ready
    } else {
        transaction
            .execute(
                "UPDATE outbox
                 SET state = 'uncertain', lease_expires_at_ms = NULL,
                     error_class = 'ambiguous', error_code = 'lease_expired_after_data',
                     updated_at_ms = ?2
                 WHERE message_id = ?1 AND state = 'in_flight'",
                params![raw_message_id, now_ms],
            )
            .map_err(map_write_error)?;
        OutboxState::Uncertain
    };
    transaction.commit().map_err(DbFailure::database)?;
    Ok(OutboxRecoveryOutcome::Recovered { message_id, state })
}

pub(crate) fn claim_outbox(
    connection: &mut Connection,
    account_id: AccountId,
    now_ms: i64,
) -> Result<OutboxClaimOutcome, DbFailure> {
    claim_outbox_inner(connection, Some(account_id), now_ms)
}

pub(crate) fn claim_next_outbox(
    connection: &mut Connection,
    now_ms: i64,
) -> Result<OutboxClaimOutcome, DbFailure> {
    claim_outbox_inner(connection, None, now_ms)
}

fn claim_outbox_inner(
    connection: &mut Connection,
    account_scope: Option<AccountId>,
    now_ms: i64,
) -> Result<OutboxClaimOutcome, DbFailure> {
    validate_timestamp(now_ms)?;
    let expires_at_ms = now_ms
        .checked_add(CLAIM_TTL_MS)
        .ok_or_else(|| DbFailure::resource_limit("outbox claim TTL overflow"))?;
    validate_timestamp(expires_at_ms)?;
    let transaction = immediate_transaction(connection)?;
    let active_lease = if let Some(account_id) = account_scope {
        transaction
            .query_row(
                "SELECT lease_expires_at_ms FROM outbox
                 WHERE account_id = ?1 AND state = 'in_flight'
                 ORDER BY lease_expires_at_ms, message_id LIMIT 1",
                [account_id.get()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(DbFailure::database)?
    } else {
        transaction
            .query_row(
                "SELECT lease_expires_at_ms FROM outbox
                 WHERE state = 'in_flight'
                 ORDER BY lease_expires_at_ms, message_id LIMIT 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(DbFailure::database)?
    };
    if let Some(wake_at_ms) = active_lease {
        transaction.commit().map_err(DbFailure::database)?;
        return Ok(OutboxClaimOutcome::Idle {
            wake_at_ms: Some(wake_at_ms),
        });
    }
    for _ in 0..MAX_CLAIM_SCAN {
        let candidate: Option<(i64, i64, i64, i64, i64, i64)> =
            if let Some(account_id) = account_scope {
                transaction
                    .query_row(
                        "SELECT message_id, account_id, artifact_generation,
                                configuration_generation, attempt_count, claim_epoch
                         FROM outbox
                         WHERE account_id = ?1
                           AND (state = 'ready' OR
                                (state = 'retry_wait' AND not_before_ms <= ?2))
                         ORDER BY coalesce(not_before_ms, created_at_ms), message_id
                         LIMIT 1",
                        params![account_id.get(), now_ms],
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
                    .optional()
                    .map_err(DbFailure::database)?
            } else {
                transaction
                    .query_row(
                        "SELECT message_id, account_id, artifact_generation,
                                configuration_generation, attempt_count, claim_epoch
                         FROM outbox
                         WHERE state = 'ready' OR
                               (state = 'retry_wait' AND not_before_ms <= ?1)
                         ORDER BY coalesce(not_before_ms, created_at_ms), message_id
                         LIMIT 1",
                        [now_ms],
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
                    .optional()
                    .map_err(DbFailure::database)?
            };
        let Some((
            raw_message_id,
            raw_account_id,
            artifact_generation,
            configuration_generation,
            attempt_count,
            claim_epoch,
        )) = candidate
        else {
            let wake_at_ms = next_outbox_wake(&transaction, account_scope)?;
            transaction.commit().map_err(DbFailure::database)?;
            return Ok(OutboxClaimOutcome::Idle { wake_at_ms });
        };
        let account_id = AccountId::new(raw_account_id)
            .map_err(|error| DbFailure::database(error.to_string()))?;
        let account_generation: Option<i64> = transaction
            .query_row(
                "SELECT account.configuration_generation
                 FROM accounts AS account
                 JOIN account_connections AS connection ON connection.account_id = account.id
                 WHERE account.id = ?1 AND account.state = 'active'
                   AND connection.smtp_state = 'configured'",
                [account_id.get()],
                |row| row.get(0),
            )
            .optional()
            .map_err(DbFailure::database)?;
        if account_generation != Some(configuration_generation) {
            terminalize_claim_candidate(
                &transaction,
                raw_message_id,
                raw_account_id,
                artifact_generation,
                configuration_generation,
                attempt_count,
                claim_epoch,
                now_ms,
                OutboxErrorClass::Configuration,
                "configuration_changed",
            )?;
            continue;
        }
        let exhausted = if attempt_count >= MAX_DELIVERY_ATTEMPTS {
            Some("attempt_limit_exhausted")
        } else if claim_epoch == i64::MAX {
            Some("claim_epoch_exhausted")
        } else {
            None
        };
        if let Some(error_code) = exhausted {
            terminalize_claim_candidate(
                &transaction,
                raw_message_id,
                raw_account_id,
                artifact_generation,
                configuration_generation,
                attempt_count,
                claim_epoch,
                now_ms,
                OutboxErrorClass::Permanent,
                error_code,
            )?;
            continue;
        }
        let updated = transaction
            .execute(
                "UPDATE outbox
                 SET state = 'in_flight', claim_epoch = ?7,
                     attempt_count = ?8, lease_expires_at_ms = ?9,
                     delivery_started = 0, error_class = NULL, error_code = NULL,
                     updated_at_ms = ?10
                 WHERE message_id = ?1 AND account_id = ?2
                   AND artifact_generation = ?3 AND configuration_generation = ?4
                   AND attempt_count = ?5 AND claim_epoch = ?6
                   AND (state = 'ready' OR
                        (state = 'retry_wait' AND not_before_ms <= ?10))",
                params![
                    raw_message_id,
                    raw_account_id,
                    artifact_generation,
                    configuration_generation,
                    attempt_count,
                    claim_epoch,
                    claim_epoch + 1,
                    attempt_count + 1,
                    expires_at_ms,
                    now_ms,
                ],
            )
            .map_err(map_write_error)?;
        if updated != 1 {
            return Err(DbFailure::conflict("outbox claim changed"));
        }
        let claim = load_claim(&transaction, MessageId::from_database(raw_message_id))?;
        transaction.commit().map_err(DbFailure::database)?;
        return Ok(OutboxClaimOutcome::Claimed(Box::new(claim)));
    }
    transaction.commit().map_err(DbFailure::database)?;
    Ok(OutboxClaimOutcome::Idle {
        wake_at_ms: Some(now_ms),
    })
}

pub(crate) fn mark_outbox_data_started(
    connection: &mut Connection,
    lease: OutboxLease,
    now_ms: i64,
) -> Result<OutboxReportOutcome, DbFailure> {
    validate_timestamp(now_ms)?;
    let expires_at_ms = now_ms
        .checked_add(CLAIM_TTL_MS)
        .ok_or_else(|| DbFailure::resource_limit("outbox DATA lease TTL overflow"))?;
    validate_timestamp(expires_at_ms)?;
    let updated = connection
        .execute(
            "UPDATE outbox
             SET delivery_started = 1, lease_expires_at_ms = ?6, updated_at_ms = ?5
             WHERE message_id = ?1 AND artifact_generation = ?2
               AND configuration_generation = ?3 AND claim_epoch = ?4
               AND state = 'in_flight' AND delivery_started = 0",
            params![
                lease.message_id.get(),
                lease.artifact_generation as i64,
                lease.configuration_generation.get(),
                lease.claim_epoch as i64,
                now_ms,
                expires_at_ms,
            ],
        )
        .map_err(map_write_error)?;
    Ok(if updated == 1 {
        OutboxReportOutcome::Applied(OutboxState::InFlight)
    } else {
        OutboxReportOutcome::Stale
    })
}

pub(crate) fn report_outbox(
    connection: &mut Connection,
    lease: OutboxLease,
    report: &OutboxReport,
    now_ms: i64,
) -> Result<OutboxReportOutcome, DbFailure> {
    validate_timestamp(now_ms)?;
    let transaction = immediate_transaction(connection)?;
    let delivery_started: Option<bool> = transaction
        .query_row(
            "SELECT delivery_started <> 0 FROM outbox
             WHERE message_id = ?1 AND artifact_generation = ?2
               AND configuration_generation = ?3 AND claim_epoch = ?4
               AND state = 'in_flight'",
            params![
                lease.message_id.get(),
                lease.artifact_generation as i64,
                lease.configuration_generation.get(),
                lease.claim_epoch as i64,
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some(delivery_started) = delivery_started else {
        transaction.commit().map_err(DbFailure::database)?;
        return Ok(OutboxReportOutcome::Stale);
    };
    let state = match report {
        OutboxReport::Delivered { delivered_at_ms } => {
            apply_delivered(&transaction, lease, *delivered_at_ms, now_ms)?;
            OutboxState::Delivered
        }
        OutboxReport::Retry { .. } if delivery_started => {
            apply_terminal(
                &transaction,
                lease,
                OutboxState::Uncertain,
                OutboxErrorClass::Ambiguous,
                "retry_requested_after_data",
                now_ms,
            )?;
            OutboxState::Uncertain
        }
        OutboxReport::Retry {
            not_before_ms,
            error_class,
            error_code,
        }
        | OutboxReport::RetryAfterRejection {
            not_before_ms,
            error_class,
            error_code,
        } => {
            transaction
                .execute(
                    "UPDATE outbox
                     SET state = 'retry_wait', lease_expires_at_ms = NULL,
                         delivery_started = 0, not_before_ms = ?5,
                         error_class = ?6, error_code = ?7, updated_at_ms = ?8
                     WHERE message_id = ?1 AND artifact_generation = ?2
                       AND configuration_generation = ?3 AND claim_epoch = ?4
                       AND state = 'in_flight'",
                    params![
                        lease.message_id.get(),
                        lease.artifact_generation as i64,
                        lease.configuration_generation.get(),
                        lease.claim_epoch as i64,
                        not_before_ms,
                        error_class.database_value(),
                        error_code.as_ref(),
                        now_ms,
                    ],
                )
                .map_err(map_write_error)?;
            OutboxState::RetryWait
        }
        OutboxReport::PermanentFailure {
            error_class,
            error_code,
        } => {
            apply_terminal(
                &transaction,
                lease,
                OutboxState::PermanentFailure,
                *error_class,
                error_code,
                now_ms,
            )?;
            OutboxState::PermanentFailure
        }
        OutboxReport::Uncertain { error_code } => {
            apply_terminal(
                &transaction,
                lease,
                OutboxState::Uncertain,
                OutboxErrorClass::Ambiguous,
                error_code,
                now_ms,
            )?;
            OutboxState::Uncertain
        }
    };
    transaction.commit().map_err(DbFailure::database)?;
    Ok(OutboxReportOutcome::Applied(state))
}

pub(crate) fn load_outbox_state(
    connection: &Connection,
    message_id: MessageId,
) -> Result<Option<OutboxState>, DbFailure> {
    connection
        .query_row(
            "SELECT state FROM outbox WHERE message_id = ?1",
            [message_id.get()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(DbFailure::database)?
        .map(|state| OutboxState::from_database(&state))
        .transpose()
}

pub(crate) fn retry_outbox(
    connection: &mut Connection,
    fence: OutboxActionFence,
    now_ms: i64,
) -> Result<OutboxReportOutcome, DbFailure> {
    validate_timestamp(now_ms)?;
    validate_artifact_generation(fence.artifact_generation)?;
    let updated = connection
        .execute(
            "UPDATE outbox
             SET state = 'ready', not_before_ms = ?5,
                 attempt_count = 0, error_class = NULL, error_code = NULL,
                 updated_at_ms = ?5
             WHERE message_id = ?1 AND account_id = ?2
               AND configuration_generation = ?3 AND artifact_generation = ?4
               AND state = 'permanent_failure' AND claim_epoch < 9223372036854775807
               AND wire_byte_count IS NOT NULL AND rfc_message_id IS NOT NULL
               AND EXISTS (
                   SELECT 1 FROM accounts AS account
                   JOIN account_connections AS connection
                     ON connection.account_id = account.id
                   WHERE account.id = outbox.account_id
                     AND account.configuration_generation = outbox.configuration_generation
                     AND account.state = 'active'
                     AND connection.smtp_state = 'configured'
               )",
            params![
                fence.message_id.get(),
                fence.account_id.get(),
                fence.configuration_generation.get(),
                fence.artifact_generation as i64,
                now_ms,
            ],
        )
        .map_err(map_write_error)?;
    Ok(if updated == 1 {
        OutboxReportOutcome::Applied(OutboxState::Ready)
    } else {
        OutboxReportOutcome::Stale
    })
}

pub(crate) fn release_failed_outbox(
    connection: &mut Connection,
    fence: OutboxActionFence,
    now_ms: i64,
) -> Result<OutboxReportOutcome, DbFailure> {
    validate_timestamp(now_ms)?;
    validate_artifact_generation(fence.artifact_generation)?;
    let transaction = immediate_transaction(connection)?;
    let released =
        release_outbox_to_draft(&transaction, fence, OutboxState::PermanentFailure, now_ms)?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(if released {
        OutboxReportOutcome::Applied(OutboxState::PermanentFailure)
    } else {
        OutboxReportOutcome::Stale
    })
}

pub(crate) fn resolve_uncertain_outbox(
    connection: &mut Connection,
    fence: OutboxActionFence,
    resolution: UncertainResolution,
    now_ms: i64,
) -> Result<OutboxReportOutcome, DbFailure> {
    validate_timestamp(now_ms)?;
    validate_artifact_generation(fence.artifact_generation)?;
    let transaction = immediate_transaction(connection)?;
    let outcome = match resolution {
        UncertainResolution::AssumeDelivered => {
            let updated = transaction
                .execute(
                    "UPDATE outbox
                     SET state = 'delivered', lease_expires_at_ms = NULL,
                         delivery_started = 0, not_before_ms = NULL,
                         error_class = NULL, error_code = NULL,
                         delivered_at_ms = ?5, updated_at_ms = ?5
                     WHERE message_id = ?1 AND account_id = ?2
                       AND configuration_generation = ?3 AND artifact_generation = ?4
                       AND state = 'uncertain'",
                    params![
                        fence.message_id.get(),
                        fence.account_id.get(),
                        fence.configuration_generation.get(),
                        fence.artifact_generation as i64,
                        now_ms,
                    ],
                )
                .map_err(map_write_error)?;
            if updated == 0 {
                OutboxReportOutcome::Stale
            } else {
                complete_delivered_message(
                    &transaction,
                    fence.message_id,
                    fence.artifact_generation,
                    fence.configuration_generation,
                    now_ms,
                )?;
                OutboxReportOutcome::Applied(OutboxState::Delivered)
            }
        }
        UncertainResolution::Release => {
            if release_outbox_to_draft(&transaction, fence, OutboxState::Uncertain, now_ms)? {
                OutboxReportOutcome::Applied(OutboxState::Uncertain)
            } else {
                OutboxReportOutcome::Stale
            }
        }
    };
    transaction.commit().map_err(DbFailure::database)?;
    Ok(outcome)
}

fn load_expired_reservation(
    transaction: &Transaction<'_>,
    now_ms: i64,
) -> Result<Option<OutboxReservation>, DbFailure> {
    type Stored = (i64, i64, i64, i64, i64, String, String, String, i64);
    let stored: Option<Stored> = transaction
        .query_row(
            "SELECT message_id, account_id, configuration_generation, draft_revision,
                    artifact_generation, reservation_token, mime_file_key, rfc_message_id,
                    reservation_expires_at_ms
             FROM outbox
             WHERE state = 'reserved' AND reservation_expires_at_ms <= ?1
             ORDER BY reservation_expires_at_ms, message_id
             LIMIT 1",
            [now_ms],
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
    stored.map(reservation_from_stored).transpose()
}

fn reservation_from_stored(
    stored: (i64, i64, i64, i64, i64, String, String, String, i64),
) -> Result<OutboxReservation, DbFailure> {
    let (
        message_id,
        account_id,
        generation,
        revision,
        artifact,
        token,
        key,
        rfc_message_id,
        expires,
    ) = stored;
    Ok(OutboxReservation {
        message_id: MessageId::from_database(message_id),
        account_id: AccountId::new(account_id)
            .map_err(|error| DbFailure::database(error.to_string()))?,
        configuration_generation: AccountGeneration::new(generation)
            .map_err(|error| DbFailure::database(error.to_string()))?,
        draft_revision: u64::try_from(revision)
            .map_err(|_| DbFailure::database("invalid outbox draft revision"))?,
        artifact_generation: u64::try_from(artifact)
            .map_err(|_| DbFailure::database("invalid outbox artifact generation"))?,
        token: OutboxReservationToken::decode(&token)?,
        file_key: FileKey::parse(&key)
            .map_err(|_| DbFailure::database("invalid outbound MIME file key"))?,
        rfc_message_id: rfc_message_id.into_boxed_str(),
        expires_at_ms: expires,
    })
}

fn load_claim(
    transaction: &Transaction<'_>,
    message_id: MessageId,
) -> Result<OutboxClaim, DbFailure> {
    type Stored = (
        i64,
        i64,
        i64,
        i64,
        String,
        String,
        i64,
        String,
        String,
        String,
        i64,
        String,
        String,
        i64,
        i64,
    );
    let stored: Stored = transaction
        .query_row(
            "SELECT outbox.account_id, outbox.configuration_generation,
                    outbox.artifact_generation, outbox.claim_epoch,
                    connection.credential_key, connection.login_name,
                    connection.smtp_port, connection.smtp_host, connection.smtp_security,
                    outbox.envelope_from, outbox.wire_byte_count,
                    outbox.rfc_message_id, outbox.mime_file_key, outbox.attempt_count,
                    outbox.lease_expires_at_ms
             FROM outbox
             JOIN account_connections AS connection ON connection.account_id = outbox.account_id
             WHERE outbox.message_id = ?1 AND outbox.state = 'in_flight'",
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
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                ))
            },
        )
        .map_err(DbFailure::database)?;
    let (
        raw_account_id,
        generation,
        artifact,
        epoch,
        credential_key,
        login_name,
        smtp_port,
        smtp_host,
        smtp_security,
        envelope_from,
        wire_bytes,
        rfc_message_id,
        mime_file_key,
        attempts,
        expires,
    ) = stored;
    let recipients = load_recipients(transaction, message_id)?;
    Ok(OutboxClaim {
        lease: OutboxLease {
            message_id,
            artifact_generation: u64::try_from(artifact)
                .map_err(|_| DbFailure::database("invalid outbox artifact generation"))?,
            claim_epoch: u64::try_from(epoch)
                .map_err(|_| DbFailure::database("invalid outbox claim epoch"))?,
            configuration_generation: AccountGeneration::new(generation)
                .map_err(|error| DbFailure::database(error.to_string()))?,
            expires_at_ms: expires,
        },
        account_id: AccountId::new(raw_account_id)
            .map_err(|error| DbFailure::database(error.to_string()))?,
        credential_key: credential_key.into_boxed_str(),
        login_name: login_name.into_boxed_str(),
        smtp_host: smtp_host.into_boxed_str(),
        smtp_port: u16::try_from(smtp_port)
            .map_err(|_| DbFailure::database("invalid SMTP port in outbox claim"))?,
        smtp_security: SmtpSecurity::from_database(&smtp_security)?,
        envelope_from: envelope_from.into_boxed_str(),
        rfc_message_id: rfc_message_id.into_boxed_str(),
        file_key: FileKey::parse(&mime_file_key)
            .map_err(|_| DbFailure::database("invalid MIME file key in outbox claim"))?,
        wire_byte_count: u64::try_from(wire_bytes)
            .map_err(|_| DbFailure::database("invalid MIME byte count in outbox claim"))?,
        recipients,
        attempt_count: u16::try_from(attempts)
            .map_err(|_| DbFailure::database("invalid outbox attempt count"))?,
    })
}

fn load_recipients(
    connection: &Connection,
    message_id: MessageId,
) -> Result<Box<[OutboxRecipient]>, DbFailure> {
    let mut statement = connection
        .prepare(
            "SELECT kind, address, display_name
             FROM outbox_recipients
             WHERE message_id = ?1
             ORDER BY CASE kind WHEN 'to' THEN 0 WHEN 'cc' THEN 1 ELSE 2 END, ordinal
             LIMIT 65",
        )
        .map_err(DbFailure::database)?;
    let rows = statement
        .query_map([message_id.get()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(DbFailure::database)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(DbFailure::database)?;
    if rows.len() > MAX_RECIPIENTS {
        return Err(DbFailure::resource_limit(
            "stored outbox recipient limit exceeded",
        ));
    }
    rows.into_iter()
        .map(|(kind, address, display_name)| {
            Ok(OutboxRecipient {
                kind: RecipientKind::from_database(&kind)?,
                address: address.into_boxed_str(),
                display_name: display_name.into_boxed_str(),
            })
        })
        .collect::<Result<Vec<_>, DbFailure>>()
        .map(Vec::into_boxed_slice)
}

fn insert_recipients(
    transaction: &Transaction<'_>,
    message_id: MessageId,
    recipients: &[OutboxRecipient],
) -> Result<(), DbFailure> {
    let mut ordinals = [0_i64; 3];
    for recipient in recipients {
        let index = match recipient.kind {
            RecipientKind::To => 0,
            RecipientKind::Cc => 1,
            RecipientKind::Bcc => 2,
        };
        transaction
            .execute(
                "INSERT INTO outbox_recipients
                     (message_id, kind, ordinal, address, display_name)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    message_id.get(),
                    recipient.kind.database_value(),
                    ordinals[index],
                    recipient.address.as_ref(),
                    recipient.display_name.as_ref(),
                ],
            )
            .map_err(map_write_error)?;
        ordinals[index] += 1;
    }
    Ok(())
}

fn apply_terminal(
    transaction: &Transaction<'_>,
    lease: OutboxLease,
    state: OutboxState,
    error_class: OutboxErrorClass,
    error_code: &str,
    now_ms: i64,
) -> Result<(), DbFailure> {
    let delivery_started = i64::from(state == OutboxState::Uncertain);
    let updated = transaction
        .execute(
            "UPDATE outbox
             SET state = ?5, lease_expires_at_ms = NULL, delivery_started = ?6,
                 not_before_ms = NULL, error_class = ?7, error_code = ?8,
                 updated_at_ms = ?9
             WHERE message_id = ?1 AND artifact_generation = ?2
               AND configuration_generation = ?3 AND claim_epoch = ?4
               AND state = 'in_flight'",
            params![
                lease.message_id.get(),
                lease.artifact_generation as i64,
                lease.configuration_generation.get(),
                lease.claim_epoch as i64,
                state.database_value(),
                delivery_started,
                error_class.database_value(),
                error_code,
                now_ms,
            ],
        )
        .map_err(map_write_error)?;
    if updated != 1 {
        return Err(DbFailure::conflict("outbox report fence changed"));
    }
    Ok(())
}

fn apply_delivered(
    transaction: &Transaction<'_>,
    lease: OutboxLease,
    delivered_at_ms: i64,
    now_ms: i64,
) -> Result<(), DbFailure> {
    validate_timestamp(delivered_at_ms)?;
    let updated = transaction
        .execute(
            "UPDATE outbox
             SET state = 'delivered', lease_expires_at_ms = NULL,
                 delivery_started = 0, not_before_ms = NULL,
                 error_class = NULL, error_code = NULL,
                 delivered_at_ms = ?5, updated_at_ms = ?6
             WHERE message_id = ?1 AND artifact_generation = ?2
               AND configuration_generation = ?3 AND claim_epoch = ?4
               AND state = 'in_flight'",
            params![
                lease.message_id.get(),
                lease.artifact_generation as i64,
                lease.configuration_generation.get(),
                lease.claim_epoch as i64,
                delivered_at_ms,
                now_ms,
            ],
        )
        .map_err(map_write_error)?;
    if updated != 1 {
        return Err(DbFailure::conflict("outbox delivery fence changed"));
    }
    complete_delivered_message(
        transaction,
        lease.message_id,
        lease.artifact_generation,
        lease.configuration_generation,
        now_ms,
    )
}

fn complete_delivered_message(
    transaction: &Transaction<'_>,
    message_id: MessageId,
    artifact_generation: u64,
    configuration_generation: AccountGeneration,
    now_ms: i64,
) -> Result<(), DbFailure> {
    let stored: Option<(i64, String)> = transaction
        .query_row(
            "SELECT account_id, mime_file_key FROM outbox
             WHERE message_id = ?1 AND artifact_generation = ?2
               AND configuration_generation = ?3 AND state = 'delivered'",
            params![
                message_id.get(),
                artifact_generation as i64,
                configuration_generation.get(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((account_id, mime_file_key)) = stored else {
        return Err(DbFailure::conflict("delivered outbox fence changed"));
    };
    let before = load_message_snapshot(transaction, message_id)?;
    transaction
        .execute(
            "DELETE FROM local_drafts WHERE message_id = ?1",
            [message_id.get()],
        )
        .map_err(map_write_error)?;
    transaction
        .execute(
            "DELETE FROM message_folders
             WHERE message_id = ?1 AND folder_id IN (
                 SELECT id FROM folders WHERE account_id = ?2 AND role = 'drafts'
             )",
            params![message_id.get(), account_id],
        )
        .map_err(map_write_error)?;
    let sent_folder = ensure_sent_folder(transaction, account_id)?;
    transaction
        .execute(
            "INSERT OR IGNORE INTO message_folders (message_id, folder_id, account_id)
             VALUES (?1, ?2, ?3)",
            params![message_id.get(), sent_folder, account_id],
        )
        .map_err(map_write_error)?;
    let after = load_message_snapshot(transaction, message_id)?;
    apply_transition(transaction, before, Some(after))?;
    transaction
        .execute(
            "INSERT OR IGNORE INTO file_gc (file_key, queued_at_ms) VALUES (?1, ?2)",
            params![mime_file_key, now_ms],
        )
        .map_err(map_write_error)?;
    let deleted = transaction
        .execute(
            "DELETE FROM outbox
             WHERE message_id = ?1 AND account_id = ?2
               AND artifact_generation = ?3 AND configuration_generation = ?4
               AND state = 'delivered'",
            params![
                message_id.get(),
                account_id,
                artifact_generation as i64,
                configuration_generation.get(),
            ],
        )
        .map_err(map_write_error)?;
    if deleted != 1 {
        return Err(DbFailure::conflict(
            "delivered outbox changed during artifact release",
        ));
    }
    Ok(())
}

fn release_outbox_to_draft(
    transaction: &Transaction<'_>,
    fence: OutboxActionFence,
    expected_state: OutboxState,
    now_ms: i64,
) -> Result<bool, DbFailure> {
    debug_assert!(matches!(
        expected_state,
        OutboxState::PermanentFailure | OutboxState::Uncertain
    ));
    let stored: Option<(String, Option<String>, Option<String>)> = transaction
        .query_row(
            "SELECT mime_file_key, rfc_message_id, error_code FROM outbox
             WHERE message_id = ?1 AND account_id = ?2
               AND configuration_generation = ?3 AND artifact_generation = ?4
               AND state = ?5",
            params![
                fence.message_id.get(),
                fence.account_id.get(),
                fence.configuration_generation.get(),
                fence.artifact_generation as i64,
                expected_state.database_value(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((file_key, rfc_message_id, error_code)) = stored else {
        return Ok(false);
    };
    let stored_lock: Option<Option<i64>> = transaction
        .query_row(
            "SELECT locked_artifact_generation FROM local_drafts WHERE message_id = ?1",
            [fence.message_id.get()],
            |row| row.get(0),
        )
        .optional()
        .map_err(DbFailure::database)?;
    match stored_lock {
        Some(Some(generation)) if generation == fence.artifact_generation as i64 => {}
        None if rfc_message_id.is_none() && error_code.as_deref() == Some("legacy_unverified") => {}
        _ => return Err(DbFailure::conflict("outbox draft lock changed")),
    }
    transaction
        .execute(
            "INSERT OR IGNORE INTO file_gc (file_key, queued_at_ms) VALUES (?1, ?2)",
            params![file_key, now_ms],
        )
        .map_err(map_write_error)?;
    if stored_lock.is_some() {
        let unlocked = transaction
            .execute(
                "UPDATE local_drafts
                 SET locked_artifact_generation = NULL
                 WHERE message_id = ?1 AND locked_artifact_generation = ?2",
                params![fence.message_id.get(), fence.artifact_generation as i64],
            )
            .map_err(map_write_error)?;
        if unlocked != 1 {
            return Err(DbFailure::conflict("outbox draft lock changed"));
        }
    }
    let deleted = transaction
        .execute(
            "DELETE FROM outbox
             WHERE message_id = ?1 AND account_id = ?2
               AND configuration_generation = ?3 AND artifact_generation = ?4
               AND state = ?5",
            params![
                fence.message_id.get(),
                fence.account_id.get(),
                fence.configuration_generation.get(),
                fence.artifact_generation as i64,
                expected_state.database_value(),
            ],
        )
        .map_err(map_write_error)?;
    if deleted != 1 {
        return Err(DbFailure::conflict(
            "terminal outbox changed during release",
        ));
    }
    Ok(true)
}

fn ensure_sent_folder(transaction: &Transaction<'_>, account_id: i64) -> Result<i64, DbFailure> {
    if let Some(folder_id) = transaction
        .query_row(
            "SELECT id FROM folders WHERE account_id = ?1 AND role = 'sent' LIMIT 1",
            [account_id],
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
             VALUES (?1, 'local:sent', 'Sent', 'sent')",
            [account_id],
        )
        .map_err(map_write_error)?;
    Ok(transaction.last_insert_rowid())
}

fn next_outbox_wake(
    transaction: &Transaction<'_>,
    account_scope: Option<AccountId>,
) -> Result<Option<i64>, DbFailure> {
    if let Some(account_id) = account_scope {
        transaction
            .query_row(
                "SELECT min(not_before_ms) FROM outbox
                 WHERE account_id = ?1 AND state = 'retry_wait'",
                [account_id.get()],
                |row| row.get(0),
            )
            .map_err(DbFailure::database)
    } else {
        transaction
            .query_row(
                "SELECT min(not_before_ms) FROM outbox WHERE state = 'retry_wait'",
                [],
                |row| row.get(0),
            )
            .map_err(DbFailure::database)
    }
}

#[allow(clippy::too_many_arguments)]
fn terminalize_claim_candidate(
    transaction: &Transaction<'_>,
    message_id: i64,
    account_id: i64,
    artifact_generation: i64,
    configuration_generation: i64,
    attempt_count: i64,
    claim_epoch: i64,
    now_ms: i64,
    error_class: OutboxErrorClass,
    error_code: &str,
) -> Result<(), DbFailure> {
    let updated = transaction
        .execute(
            "UPDATE outbox
             SET state = 'permanent_failure', lease_expires_at_ms = NULL,
                 delivery_started = 0, not_before_ms = NULL,
                 error_class = ?7, error_code = ?8, updated_at_ms = ?9
             WHERE message_id = ?1 AND account_id = ?2
               AND artifact_generation = ?3 AND configuration_generation = ?4
               AND attempt_count = ?5 AND claim_epoch = ?6
               AND (state = 'ready' OR
                    (state = 'retry_wait' AND not_before_ms <= ?9))",
            params![
                message_id,
                account_id,
                artifact_generation,
                configuration_generation,
                attempt_count,
                claim_epoch,
                error_class.database_value(),
                error_code,
                now_ms,
            ],
        )
        .map_err(map_write_error)?;
    if updated != 1 {
        return Err(DbFailure::conflict(
            "outbox candidate changed while being terminalized",
        ));
    }
    Ok(())
}

fn immediate_transaction(connection: &mut Connection) -> Result<Transaction<'_>, DbFailure> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)
}

fn validate_artifact_generation(artifact_generation: u64) -> Result<(), DbFailure> {
    if artifact_generation == 0 || artifact_generation > i64::MAX as u64 {
        return Err(DbFailure::resource_limit(
            "outbox artifact generation is outside SQLite bounds",
        ));
    }
    Ok(())
}

fn stored_recipient_count(value: i64) -> Result<u32, DbFailure> {
    u32::try_from(value).map_err(|_| DbFailure::database("invalid outbox recipient count"))
}

fn validate_recipients(recipients: &[OutboxRecipient]) -> Result<(), DbFailure> {
    if recipients.len() > MAX_RECIPIENTS {
        return Err(DbFailure::resource_limit(format!(
            "outbox exceeds the {MAX_RECIPIENTS}-recipient limit"
        )));
    }
    Ok(())
}

fn validate_wire_bytes(bytes: u64) -> Result<(), DbFailure> {
    if (1..=MAX_OUTBOUND_MIME_BYTES).contains(&bytes) {
        Ok(())
    } else {
        Err(DbFailure::resource_limit(format!(
            "outbound MIME must contain between 1 and {MAX_OUTBOUND_MIME_BYTES} bytes"
        )))
    }
}

fn validate_error_code(error_code: &str) -> Result<(), DbFailure> {
    validate_text(error_code, MAX_ERROR_CODE_BYTES, false, "outbox error code")
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
            "outbox timestamp is outside SQLite bounds",
        ))
    }
}

fn decode_hex(value: u8) -> Result<u8, DbFailure> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(DbFailure::database("invalid outbox reservation token")),
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
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
        content::{ContentStaging, ReservedFileObservation},
        store::sqlite::{
            domain::FailureKind,
            draft::{DraftUpdate, NewDraft, create_draft, load_draft, update_draft},
            migrations::migrate,
        },
    };

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "nivalis-outbox-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
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
        let mut connection = Connection::open_in_memory().expect("open outbox test database");
        migrate(&mut connection).expect("migrate outbox test database");
        connection
            .execute(
                "INSERT INTO accounts
                     (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (1, 'imap', 'account-1', 'Account', 'sender@example.test', 'active', 0)",
                [],
            )
            .expect("insert outbox account");
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
            .expect("insert outbox connection");
        connection
    }

    fn body(value: u8) -> FileKey {
        FileKey::parse(&format!("body/{value:032x}.txt")).expect("valid draft body key")
    }

    fn create_test_draft(connection: &mut Connection, value: u8, now_ms: i64) -> MessageId {
        create_draft(
            connection,
            &NewDraft::new(
                AccountId::new(1).unwrap(),
                AccountGeneration::new(1).unwrap(),
                &format!("local:draft-{value}"),
                "Subject",
                "Preview",
                "Body",
                body(value),
                4,
                vec![],
                now_ms,
            )
            .unwrap(),
        )
        .unwrap()
        .message_id
    }

    fn reserve(
        connection: &mut Connection,
        message_id: MessageId,
        token_byte: u8,
        now_ms: i64,
    ) -> OutboxReservation {
        reserve_with_generation(connection, message_id, token_byte, now_ms, 1)
    }

    fn reserve_with_generation(
        connection: &mut Connection,
        message_id: MessageId,
        token_byte: u8,
        now_ms: i64,
        generation: i64,
    ) -> OutboxReservation {
        reserve_outbox(
            connection,
            &OutboxReserveRequest::new(
                message_id,
                AccountId::new(1).unwrap(),
                AccountGeneration::new(generation).unwrap(),
                0,
                OutboxReservationToken::new([token_byte; 16]),
                &format!("<message-{token_byte}@example.test>"),
                vec![
                    OutboxRecipient::new(RecipientKind::To, "recipient@example.test", "Recipient")
                        .unwrap(),
                ],
                now_ms,
                now_ms + 1_000,
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn action_fence(message_id: MessageId) -> OutboxActionFence {
        OutboxActionFence::new(
            message_id,
            AccountId::new(1).unwrap(),
            AccountGeneration::new(1).unwrap(),
            1,
        )
        .unwrap()
    }

    #[test]
    fn reservation_locks_draft_and_recovers_both_file_crash_windows() {
        let mut connection = database();
        let message_id = create_test_draft(&mut connection, 1, 100);
        let reservation = reserve(&mut connection, message_id, 0x11, 200);
        let locked = load_draft(&connection, message_id).unwrap().unwrap();
        assert_eq!(locked.locked_artifact_generation, Some(1));
        let update = DraftUpdate::new(
            message_id,
            AccountId::new(1).unwrap(),
            AccountGeneration::new(1).unwrap(),
            0,
            "Changed",
            "Changed",
            "Changed",
            body(2),
            7,
            vec![],
            300,
        )
        .unwrap();
        assert_eq!(
            update_draft(&mut connection, &update).unwrap_err().kind,
            FailureKind::Conflict
        );

        let root = TestRoot::new();
        let staging = ContentStaging::open(root.path().to_path_buf()).unwrap();
        let token = reservation.token.encoded();
        let partial = root
            .path()
            .join("outbound")
            .join(format!(".{token}.eml.part"));
        fs::write(&partial, b"partial MIME").unwrap();
        assert_eq!(
            staging
                .observe_reserved_file(&reservation.file_key, MAX_OUTBOUND_MIME_BYTES)
                .unwrap(),
            ReservedFileObservation::Missing
        );
        assert!(!partial.exists());
        let OutboxRecoveryOutcome::Reservation(expired) =
            recover_outbox(&mut connection, 1_201).unwrap()
        else {
            panic!("expected expired reservation");
        };
        assert_eq!(expired, reservation);
        assert_eq!(expired.rfc_message_id.as_ref(), "<message-17@example.test>");
        let ReservationRecovery::Rebuild(renewed) = recover_reservation(
            &mut connection,
            &reservation,
            ArtifactObservation::Missing,
            1_201,
        )
        .unwrap() else {
            panic!("missing artifact must be rebuilt");
        };

        let staged = staging
            .stage_writer_at(
                &renewed.file_key,
                MAX_OUTBOUND_MIME_BYTES as usize,
                |writer| {
                    writer.write_all(b"From: sender@example.test\r\n")?;
                    writer.write_all(b"To: recipient@example.test\r\n\r\n")?;
                    writer.write_all(b"body")
                },
            )
            .unwrap();
        let wire_bytes = staged.byte_count();
        let mut published = staged.publish().unwrap();
        published.retain();
        assert_eq!(published.key(), &renewed.file_key);
        assert_eq!(
            staging
                .observe_reserved_file(&renewed.file_key, MAX_OUTBOUND_MIME_BYTES)
                .unwrap(),
            ReservedFileObservation::Published {
                byte_count: wire_bytes
            }
        );
        assert_eq!(
            recover_reservation(
                &mut connection,
                &renewed,
                ArtifactObservation::Published {
                    byte_count: wire_bytes,
                },
                1_202,
            )
            .unwrap(),
            ReservationRecovery::Ready
        );
        assert_eq!(
            load_outbox_state(&connection, message_id).unwrap(),
            Some(OutboxState::Ready)
        );
    }

    #[test]
    fn claim_data_fence_prevents_automatic_retry_after_ambiguous_delivery() {
        let mut connection = database();
        let message_id = create_test_draft(&mut connection, 3, 100);
        let reservation = reserve(&mut connection, message_id, 0x22, 200);
        assert_eq!(
            finalize_outbox(&mut connection, &reservation, 128, 300).unwrap(),
            OutboxReportOutcome::Applied(OutboxState::Ready)
        );
        let OutboxClaimOutcome::Claimed(claim) = claim_next_outbox(&mut connection, 400).unwrap()
        else {
            panic!("expected global outbox claim");
        };
        let mut stale = claim.lease;
        stale.claim_epoch += 1;
        assert_eq!(
            mark_outbox_data_started(&mut connection, stale, 450).unwrap(),
            OutboxReportOutcome::Stale
        );
        assert_eq!(
            mark_outbox_data_started(&mut connection, claim.lease, 450).unwrap(),
            OutboxReportOutcome::Applied(OutboxState::InFlight)
        );
        assert_eq!(
            report_outbox(
                &mut connection,
                claim.lease,
                &OutboxReport::retry(1_000, OutboxErrorClass::Network, "disconnect").unwrap(),
                500,
            )
            .unwrap(),
            OutboxReportOutcome::Applied(OutboxState::Uncertain)
        );
        assert_eq!(
            retry_outbox(&mut connection, action_fence(message_id), 600).unwrap(),
            OutboxReportOutcome::Stale
        );
        assert_eq!(
            release_failed_outbox(&mut connection, action_fence(message_id), 600).unwrap(),
            OutboxReportOutcome::Stale
        );
    }

    #[test]
    fn confirmed_transient_rejection_retries_with_claim_generation_fences() {
        let mut connection = database();
        let message_id = create_test_draft(&mut connection, 31, 100);
        let reservation = reserve(&mut connection, message_id, 0x92, 200);
        finalize_outbox(&mut connection, &reservation, 128, 300).unwrap();
        let OutboxClaimOutcome::Claimed(claim) = claim_next_outbox(&mut connection, 400).unwrap()
        else {
            panic!("expected global outbox claim");
        };
        assert_eq!(
            mark_outbox_data_started(&mut connection, claim.lease, 450).unwrap(),
            OutboxReportOutcome::Applied(OutboxState::InFlight)
        );

        let report = OutboxReport::retry_after_rejection(
            1_000,
            OutboxErrorClass::Network,
            "smtp_transient_rejection",
        )
        .unwrap();
        let mut stale_generation = claim.lease;
        stale_generation.artifact_generation += 1;
        assert_eq!(
            report_outbox(&mut connection, stale_generation, &report, 500).unwrap(),
            OutboxReportOutcome::Stale
        );
        let mut stale_claim = claim.lease;
        stale_claim.claim_epoch += 1;
        assert_eq!(
            report_outbox(&mut connection, stale_claim, &report, 500).unwrap(),
            OutboxReportOutcome::Stale
        );
        assert_eq!(
            load_outbox_state(&connection, message_id).unwrap(),
            Some(OutboxState::InFlight)
        );
        assert_eq!(
            report_outbox(&mut connection, claim.lease, &report, 500).unwrap(),
            OutboxReportOutcome::Applied(OutboxState::RetryWait)
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT state, delivery_started, not_before_ms, error_class, error_code
                     FROM outbox WHERE message_id = ?1",
                    [message_id.get()],
                    |row| Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?
                    )),
                )
                .unwrap(),
            (
                "retry_wait".to_owned(),
                0,
                1_000,
                "network".to_owned(),
                "smtp_transient_rejection".to_owned(),
            )
        );
    }

    #[test]
    fn retries_safe_failures_and_expired_leases_but_releases_only_failed_items() {
        let mut connection = database();
        let message_id = create_test_draft(&mut connection, 4, 100);
        let reservation = reserve(&mut connection, message_id, 0x33, 200);
        finalize_outbox(&mut connection, &reservation, 128, 300).unwrap();
        let OutboxClaimOutcome::Claimed(first) = claim_next_outbox(&mut connection, 400).unwrap()
        else {
            panic!("expected first claim");
        };
        assert_eq!(
            report_outbox(
                &mut connection,
                first.lease,
                &OutboxReport::retry(900, OutboxErrorClass::Network, "offline").unwrap(),
                500,
            )
            .unwrap(),
            OutboxReportOutcome::Applied(OutboxState::RetryWait)
        );
        assert_eq!(
            claim_next_outbox(&mut connection, 800).unwrap(),
            OutboxClaimOutcome::Idle {
                wake_at_ms: Some(900)
            }
        );
        let OutboxClaimOutcome::Claimed(second) = claim_next_outbox(&mut connection, 900).unwrap()
        else {
            panic!("expected retry claim");
        };
        assert_eq!(second.lease.claim_epoch, first.lease.claim_epoch + 1);
        assert_eq!(
            report_outbox(
                &mut connection,
                second.lease,
                &OutboxReport::permanent_failure(
                    OutboxErrorClass::Authentication,
                    "authentication",
                )
                .unwrap(),
                950,
            )
            .unwrap(),
            OutboxReportOutcome::Applied(OutboxState::PermanentFailure)
        );
        assert_eq!(
            retry_outbox(
                &mut connection,
                OutboxActionFence::new(
                    message_id,
                    AccountId::new(1).unwrap(),
                    AccountGeneration::new(2).unwrap(),
                    1,
                )
                .unwrap(),
                999,
            )
            .unwrap(),
            OutboxReportOutcome::Stale
        );
        assert_eq!(
            retry_outbox(&mut connection, action_fence(message_id), 1_000).unwrap(),
            OutboxReportOutcome::Applied(OutboxState::Ready)
        );
        let OutboxClaimOutcome::Claimed(third) = claim_next_outbox(&mut connection, 1_001).unwrap()
        else {
            panic!("expected retried claim");
        };
        assert_eq!(third.attempt_count, 1);
        report_outbox(
            &mut connection,
            third.lease,
            &OutboxReport::permanent_failure(OutboxErrorClass::Permanent, "rejected").unwrap(),
            1_100,
        )
        .unwrap();
        assert_eq!(
            release_failed_outbox(
                &mut connection,
                OutboxActionFence::new(
                    message_id,
                    AccountId::new(1).unwrap(),
                    AccountGeneration::new(1).unwrap(),
                    2,
                )
                .unwrap(),
                1_199,
            )
            .unwrap(),
            OutboxReportOutcome::Stale
        );
        assert_eq!(
            release_failed_outbox(&mut connection, action_fence(message_id), 1_200).unwrap(),
            OutboxReportOutcome::Applied(OutboxState::PermanentFailure)
        );
        assert_eq!(load_outbox_state(&connection, message_id).unwrap(), None);
        assert_eq!(
            load_draft(&connection, message_id)
                .unwrap()
                .unwrap()
                .locked_artifact_generation,
            None
        );
        let queued: i64 = connection
            .query_row(
                "SELECT count(*) FROM file_gc WHERE file_key = ?1",
                [reservation.file_key.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(queued, 1);
    }

    #[test]
    fn releases_migrated_terminal_rows_without_accepting_a_missing_current_draft() {
        let mut connection = database();
        let legacy_message = create_test_draft(&mut connection, 63, 100);
        let legacy_reservation = reserve(&mut connection, legacy_message, 0x72, 200);
        finalize_outbox(&mut connection, &legacy_reservation, 128, 300).unwrap();
        connection
            .execute(
                "UPDATE outbox
                 SET state = 'permanent_failure', rfc_message_id = NULL,
                     error_class = 'configuration', error_code = 'legacy_unverified'
                 WHERE message_id = ?1",
                [legacy_message.get()],
            )
            .unwrap();
        connection
            .execute(
                "DELETE FROM local_drafts WHERE message_id = ?1",
                [legacy_message.get()],
            )
            .unwrap();
        assert_eq!(
            release_failed_outbox(&mut connection, action_fence(legacy_message), 400).unwrap(),
            OutboxReportOutcome::Applied(OutboxState::PermanentFailure)
        );
        assert_eq!(
            load_outbox_state(&connection, legacy_message).unwrap(),
            None
        );

        let current_message = create_test_draft(&mut connection, 64, 500);
        let current_reservation = reserve(&mut connection, current_message, 0x73, 600);
        finalize_outbox(&mut connection, &current_reservation, 128, 700).unwrap();
        connection
            .execute(
                "UPDATE outbox
                 SET state = 'permanent_failure', error_class = 'permanent',
                     error_code = 'rejected'
                 WHERE message_id = ?1",
                [current_message.get()],
            )
            .unwrap();
        connection
            .execute(
                "DELETE FROM local_drafts WHERE message_id = ?1",
                [current_message.get()],
            )
            .unwrap();
        assert_eq!(
            release_failed_outbox(&mut connection, action_fence(current_message), 800)
                .unwrap_err()
                .kind,
            FailureKind::Conflict
        );
        assert_eq!(
            load_outbox_state(&connection, current_message).unwrap(),
            Some(OutboxState::PermanentFailure)
        );
    }

    #[test]
    fn expired_leases_retry_before_data_and_become_uncertain_after_data() {
        let mut connection = database();
        let message_id = create_test_draft(&mut connection, 5, 100);
        let reservation = reserve(&mut connection, message_id, 0x44, 200);
        finalize_outbox(&mut connection, &reservation, 128, 300).unwrap();
        let OutboxClaimOutcome::Claimed(first) = claim_next_outbox(&mut connection, 400).unwrap()
        else {
            panic!("expected first claim");
        };
        assert_eq!(
            recover_outbox(&mut connection, first.lease.expires_at_ms + 1).unwrap(),
            OutboxRecoveryOutcome::Recovered {
                message_id,
                state: OutboxState::Ready,
            }
        );
        let OutboxClaimOutcome::Claimed(second) =
            claim_next_outbox(&mut connection, first.lease.expires_at_ms + 2).unwrap()
        else {
            panic!("expected recovered claim");
        };
        mark_outbox_data_started(&mut connection, second.lease, first.lease.expires_at_ms + 3)
            .unwrap();
        assert_eq!(
            recover_outbox(
                &mut connection,
                second.lease.expires_at_ms + CLAIM_TTL_MS + 1
            )
            .unwrap(),
            OutboxRecoveryOutcome::Recovered {
                message_id,
                state: OutboxState::Uncertain,
            }
        );
    }

    #[test]
    fn summary_query_is_bounded_and_carries_terminal_action_fences() {
        let mut connection = database();
        let failed_message = create_test_draft(&mut connection, 6, 100);
        let queued_message = create_test_draft(&mut connection, 7, 101);
        let failed_reservation = reserve(&mut connection, failed_message, 0x51, 200);
        let queued_reservation = reserve(&mut connection, queued_message, 0x52, 201);
        finalize_outbox(&mut connection, &failed_reservation, 128, 300).unwrap();
        finalize_outbox(&mut connection, &queued_reservation, 128, 301).unwrap();
        let OutboxClaimOutcome::Claimed(claim) = claim_next_outbox(&mut connection, 400).unwrap()
        else {
            panic!("expected first outbox claim");
        };
        report_outbox(
            &mut connection,
            claim.lease,
            &OutboxReport::permanent_failure(
                OutboxErrorClass::Authentication,
                "authentication_failed",
            )
            .unwrap(),
            500,
        )
        .unwrap();

        let first_page = load_outbox_summaries(&connection, 1).unwrap();
        assert!(first_page.has_more);
        assert_eq!(first_page.items.len(), 1);
        let failed = &first_page.items[0];
        assert_eq!(failed.message_id, failed_message);
        assert_eq!(failed.state, OutboxState::PermanentFailure);
        assert_eq!(&*failed.subject, "Subject");
        assert_eq!(
            failed.recipients.first_to_address.as_deref(),
            Some("recipient@example.test")
        );
        assert_eq!(failed.recipients.to_count, 1);
        assert_eq!(failed.recipients.cc_count, 0);
        assert_eq!(failed.recipients.bcc_count, 0);
        assert_eq!(failed.error_class, Some(OutboxErrorClass::Authentication));
        assert_eq!(failed.error_code.as_deref(), Some("authentication_failed"));
        assert_eq!(failed.action_fence(), action_fence(failed_message));

        let full_page = load_outbox_summaries(&connection, MAX_OUTBOX_SUMMARIES).unwrap();
        assert!(!full_page.has_more);
        assert_eq!(full_page.items.len(), 2);
        assert_eq!(
            load_outbox_summaries(&connection, 0).unwrap_err().kind,
            FailureKind::ResourceLimit
        );
        assert_eq!(
            load_outbox_summaries(&connection, MAX_OUTBOX_SUMMARIES + 1)
                .unwrap_err()
                .kind,
            FailureKind::ResourceLimit
        );
    }

    #[test]
    fn claim_terminalizes_exhausted_heads_and_claims_following_mail() {
        let mut connection = database();
        let attempts_exhausted = create_test_draft(&mut connection, 8, 100);
        let epoch_exhausted = create_test_draft(&mut connection, 9, 101);
        let deliverable = create_test_draft(&mut connection, 10, 102);
        for (message_id, token, created_at_ms) in [
            (attempts_exhausted, 0x61, 200),
            (epoch_exhausted, 0x62, 201),
            (deliverable, 0x63, 202),
        ] {
            let reservation = reserve(&mut connection, message_id, token, created_at_ms);
            finalize_outbox(&mut connection, &reservation, 128, created_at_ms + 100).unwrap();
        }
        connection
            .execute(
                "UPDATE outbox SET attempt_count = ?2 WHERE message_id = ?1",
                params![attempts_exhausted.get(), MAX_DELIVERY_ATTEMPTS],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE outbox SET claim_epoch = ?2 WHERE message_id = ?1",
                params![epoch_exhausted.get(), i64::MAX],
            )
            .unwrap();

        let OutboxClaimOutcome::Claimed(claim) = claim_next_outbox(&mut connection, 500).unwrap()
        else {
            panic!("expected claim after exhausted queue heads");
        };
        assert_eq!(claim.lease.message_id, deliverable);
        let exhausted: Vec<(i64, String, String)> = {
            let mut statement = connection
                .prepare(
                    "SELECT message_id, state, error_code FROM outbox
                     WHERE message_id IN (?1, ?2) ORDER BY message_id",
                )
                .unwrap();
            statement
                .query_map(
                    params![attempts_exhausted.get(), epoch_exhausted.get()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(
            exhausted,
            vec![
                (
                    attempts_exhausted.get(),
                    "permanent_failure".to_owned(),
                    "attempt_limit_exhausted".to_owned(),
                ),
                (
                    epoch_exhausted.get(),
                    "permanent_failure".to_owned(),
                    "claim_epoch_exhausted".to_owned(),
                ),
            ]
        );
        assert_eq!(
            retry_outbox(&mut connection, action_fence(attempts_exhausted), 501).unwrap(),
            OutboxReportOutcome::Applied(OutboxState::Ready)
        );
        let reset_attempts: i64 = connection
            .query_row(
                "SELECT attempt_count FROM outbox WHERE message_id = ?1",
                [attempts_exhausted.get()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reset_attempts, 0);
        assert_eq!(
            retry_outbox(&mut connection, action_fence(epoch_exhausted), 501).unwrap(),
            OutboxReportOutcome::Stale
        );
        assert_eq!(
            release_failed_outbox(&mut connection, action_fence(epoch_exhausted), 502).unwrap(),
            OutboxReportOutcome::Applied(OutboxState::PermanentFailure)
        );
    }

    #[test]
    fn claim_scan_limit_yields_immediately_then_reaches_valid_mail() {
        let mut connection = database();
        let mut stale_messages = Vec::with_capacity(MAX_CLAIM_SCAN);
        for index in 0..=MAX_CLAIM_SCAN {
            let value = u8::try_from(index + 20).unwrap();
            let message_id = create_test_draft(&mut connection, value, 100 + index as i64);
            if index < MAX_CLAIM_SCAN {
                let reservation = reserve(&mut connection, message_id, value, 1_000 + index as i64);
                finalize_outbox(&mut connection, &reservation, 128, 2_000 + index as i64).unwrap();
                stale_messages.push(message_id);
            } else {
                connection
                    .execute(
                        "UPDATE accounts SET configuration_generation = 2 WHERE id = 1",
                        [],
                    )
                    .unwrap();
                let reservation =
                    reserve_with_generation(&mut connection, message_id, value, 10_000, 2);
                finalize_outbox(&mut connection, &reservation, 128, 10_001).unwrap();
            }
        }

        assert_eq!(
            claim_next_outbox(&mut connection, 20_000).unwrap(),
            OutboxClaimOutcome::Idle {
                wake_at_ms: Some(20_000)
            }
        );
        let terminalized: i64 = connection
            .query_row(
                "SELECT count(*) FROM outbox
                 WHERE state = 'permanent_failure'
                   AND error_code = 'configuration_changed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(terminalized, MAX_CLAIM_SCAN as i64);
        let OutboxClaimOutcome::Claimed(claim) =
            claim_next_outbox(&mut connection, 20_000).unwrap()
        else {
            panic!("expected valid mail after bounded continuation");
        };
        assert!(!stale_messages.contains(&claim.lease.message_id));
        assert_eq!(claim.lease.configuration_generation.get(), 2);
    }

    #[test]
    fn delivered_mail_releases_mime_and_rebuilds_mailbox_statistics() {
        let mut connection = database();
        let message_id = create_test_draft(&mut connection, 60, 100);
        let reservation = reserve(&mut connection, message_id, 0x71, 200);
        finalize_outbox(&mut connection, &reservation, 128, 300).unwrap();
        let OutboxClaimOutcome::Claimed(claim) = claim_next_outbox(&mut connection, 400).unwrap()
        else {
            panic!("expected delivery claim");
        };
        mark_outbox_data_started(&mut connection, claim.lease, 450).unwrap();
        assert_eq!(
            report_outbox(
                &mut connection,
                claim.lease,
                &OutboxReport::delivered(500).unwrap(),
                500,
            )
            .unwrap(),
            OutboxReportOutcome::Applied(OutboxState::Delivered)
        );

        assert_eq!(load_outbox_state(&connection, message_id).unwrap(), None);
        assert_eq!(load_draft(&connection, message_id).unwrap(), None);
        let queued: i64 = connection
            .query_row(
                "SELECT count(*) FROM file_gc WHERE file_key = ?1",
                [reservation.file_key.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(queued, 1);
        let stats: (i64, i64, bool) = connection
            .query_row(
                "SELECT drafts_total, sent_total, dirty FROM account_mailbox_stats
                 WHERE account_id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stats, (0, 1, false));
    }

    #[test]
    fn uncertain_resolution_is_fenced_and_never_requeues() {
        for (value, resolution) in [
            (61, UncertainResolution::AssumeDelivered),
            (62, UncertainResolution::Release),
        ] {
            let mut connection = database();
            let message_id = create_test_draft(&mut connection, value, 100);
            let reservation = reserve(&mut connection, message_id, value, 200);
            finalize_outbox(&mut connection, &reservation, 128, 300).unwrap();
            let OutboxClaimOutcome::Claimed(claim) =
                claim_next_outbox(&mut connection, 400).unwrap()
            else {
                panic!("expected uncertain delivery claim");
            };
            mark_outbox_data_started(&mut connection, claim.lease, 450).unwrap();
            report_outbox(
                &mut connection,
                claim.lease,
                &OutboxReport::uncertain("connection_lost").unwrap(),
                500,
            )
            .unwrap();
            assert_eq!(
                claim_next_outbox(&mut connection, 600).unwrap(),
                OutboxClaimOutcome::Idle { wake_at_ms: None }
            );

            for stale_fence in [
                OutboxActionFence::new(
                    message_id,
                    AccountId::new(2).unwrap(),
                    AccountGeneration::new(1).unwrap(),
                    1,
                )
                .unwrap(),
                OutboxActionFence::new(
                    message_id,
                    AccountId::new(1).unwrap(),
                    AccountGeneration::new(2).unwrap(),
                    1,
                )
                .unwrap(),
                OutboxActionFence::new(
                    message_id,
                    AccountId::new(1).unwrap(),
                    AccountGeneration::new(1).unwrap(),
                    2,
                )
                .unwrap(),
            ] {
                assert_eq!(
                    resolve_uncertain_outbox(&mut connection, stale_fence, resolution, 601,)
                        .unwrap(),
                    OutboxReportOutcome::Stale
                );
            }
            assert_eq!(
                load_outbox_state(&connection, message_id).unwrap(),
                Some(OutboxState::Uncertain)
            );
            let expected = match resolution {
                UncertainResolution::AssumeDelivered => OutboxState::Delivered,
                UncertainResolution::Release => OutboxState::Uncertain,
            };
            assert_eq!(
                resolve_uncertain_outbox(
                    &mut connection,
                    action_fence(message_id),
                    resolution,
                    700,
                )
                .unwrap(),
                OutboxReportOutcome::Applied(expected)
            );
            assert_eq!(load_outbox_state(&connection, message_id).unwrap(), None);
            assert_eq!(
                claim_next_outbox(&mut connection, 701).unwrap(),
                OutboxClaimOutcome::Idle { wake_at_ms: None }
            );
            let queued: i64 = connection
                .query_row(
                    "SELECT count(*) FROM file_gc WHERE file_key = ?1",
                    [reservation.file_key.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(queued, 1);
            let draft = load_draft(&connection, message_id).unwrap();
            let stats: (i64, i64, bool) = connection
                .query_row(
                    "SELECT drafts_total, sent_total, dirty FROM account_mailbox_stats
                     WHERE account_id = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            match resolution {
                UncertainResolution::AssumeDelivered => {
                    assert!(draft.is_none());
                    assert_eq!(stats, (0, 1, false));
                }
                UncertainResolution::Release => {
                    assert_eq!(draft.unwrap().locked_artifact_generation, None);
                    assert_eq!(stats, (1, 0, false));
                }
            }
        }
    }
}
