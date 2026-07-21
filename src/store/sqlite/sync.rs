use std::{collections::HashSet, str};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{
    account::AccountGeneration,
    domain::{AccountId, DbFailure, MessageId},
    stats,
};

const MAX_RECEIVE_PAGE: usize = 50;
const MAX_SENDER_BYTES: usize = 320;
const MAX_SUBJECT_BYTES: usize = 998;
const MAX_PREVIEW_BYTES: usize = 2_048;
const MIN_TIMESTAMP_MS: i64 = -62_135_596_800_000;
const MAX_TIMESTAMP_MS: i64 = 253_402_300_799_999;
const INBOX_REMOTE_KEY: &str = "inbox";
const KNOWN_FLAG_BITS: u8 = InboxFlags::SEEN | InboxFlags::FLAGGED;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InboxCheckpoint {
    pub(crate) expected_cursor: Option<u32>,
    pub(crate) uid_validity: Option<u32>,
    pub(crate) history_cursor: Option<u32>,
    pub(crate) history_complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InboxCheckpointOutcome {
    Current(InboxCheckpoint),
    Stale,
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImapMessageContentTarget {
    pub(crate) uid_validity: u32,
    pub(crate) uid: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImapMessageContentTargetOutcome {
    Current(ImapMessageContentTarget),
    AlreadyAvailable,
    Stale,
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActiveImapAccountFence {
    Current,
    Stale,
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InboxFlags(u8);

impl InboxFlags {
    pub(crate) const SEEN: u8 = 1 << 0;
    pub(crate) const FLAGGED: u8 = 1 << 1;

    pub(crate) fn from_bits(bits: u8) -> Result<Self, InboxValidationError> {
        if bits & !KNOWN_FLAG_BITS != 0 {
            return Err(InboxValidationError::Flags(bits));
        }
        Ok(Self(bits))
    }

    pub(crate) fn new(seen: bool, flagged: bool) -> Self {
        let bits = (u8::from(seen) * Self::SEEN) | (u8::from(flagged) * Self::FLAGGED);
        Self(bits)
    }

    fn seen(self) -> bool {
        self.0 & Self::SEEN != 0
    }

    fn flagged(self) -> bool {
        self.0 & Self::FLAGGED != 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InboxEnvelope {
    uid: u32,
    sender_name: Box<str>,
    sender_address: Box<str>,
    subject: Box<str>,
    preview: Box<str>,
    received_at_ms: i64,
    flags: InboxFlags,
    has_attachment: bool,
}

impl InboxEnvelope {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        uid: u32,
        sender_name: &[u8],
        sender_address: &[u8],
        subject: &[u8],
        preview: &[u8],
        received_at_ms: i64,
        flags: InboxFlags,
        has_attachment: bool,
    ) -> Result<Self, InboxValidationError> {
        if uid == 0 {
            return Err(InboxValidationError::Uid);
        }
        validate_timestamp(received_at_ms)?;
        Ok(Self {
            uid,
            sender_name: validate_text("sender name", sender_name, MAX_SENDER_BYTES)?,
            sender_address: validate_text("sender address", sender_address, MAX_SENDER_BYTES)?,
            subject: validate_text("subject", subject, MAX_SUBJECT_BYTES)?,
            preview: validate_text("preview", preview, MAX_PREVIEW_BYTES)?,
            received_at_ms,
            flags,
            has_attachment,
        })
    }

    pub(crate) fn uid(&self) -> u32 {
        self.uid
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InboxReceivePage {
    account_id: AccountId,
    expected_generation: AccountGeneration,
    uid_validity: u32,
    progress: InboxReceiveProgress,
    messages: Box<[InboxEnvelope]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InboxReceiveProgress {
    Forward {
        expected_cursor: Option<u32>,
        scanned_through_uid: Option<u32>,
        bootstrap_history: Option<InboxHistoryProgress>,
    },
    History {
        expected_cursor: u32,
        expected_history_cursor: u32,
        next_history_cursor: Option<u32>,
        history_complete: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InboxHistoryProgress {
    cursor: Option<u32>,
    complete: bool,
}

impl InboxReceivePage {
    pub(crate) fn new(
        account_id: AccountId,
        expected_generation: AccountGeneration,
        expected_cursor: Option<u32>,
        uid_validity: u32,
        scanned_through_uid: Option<u32>,
        messages: Vec<InboxEnvelope>,
    ) -> Result<Self, InboxValidationError> {
        Self::new_forward(
            account_id,
            expected_generation,
            expected_cursor,
            uid_validity,
            scanned_through_uid,
            None,
            messages,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_bootstrap(
        account_id: AccountId,
        expected_generation: AccountGeneration,
        uid_validity: u32,
        scanned_through_uid: Option<u32>,
        history_cursor: Option<u32>,
        history_complete: bool,
        messages: Vec<InboxEnvelope>,
    ) -> Result<Self, InboxValidationError> {
        if history_complete != history_cursor.is_none() || history_cursor == Some(0) {
            return Err(InboxValidationError::HistoryProgress);
        }
        Self::new_forward(
            account_id,
            expected_generation,
            None,
            uid_validity,
            scanned_through_uid,
            Some(InboxHistoryProgress {
                cursor: history_cursor,
                complete: history_complete,
            }),
            messages,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_forward(
        account_id: AccountId,
        expected_generation: AccountGeneration,
        expected_cursor: Option<u32>,
        uid_validity: u32,
        scanned_through_uid: Option<u32>,
        bootstrap_history: Option<InboxHistoryProgress>,
        messages: Vec<InboxEnvelope>,
    ) -> Result<Self, InboxValidationError> {
        if uid_validity == 0 {
            return Err(InboxValidationError::UidValidity);
        }
        if messages.len() > MAX_RECEIVE_PAGE {
            return Err(InboxValidationError::PageSize {
                found: messages.len(),
                maximum: MAX_RECEIVE_PAGE,
            });
        }
        if let (Some(scanned_through_uid), Some(cursor)) = (scanned_through_uid, expected_cursor)
            && scanned_through_uid < cursor
        {
            return Err(InboxValidationError::ScanBoundaryBeforeCursor {
                scanned_through_uid,
                cursor,
            });
        }
        let lower_bound = expected_cursor.unwrap_or(0);
        let mut unique = HashSet::with_capacity(messages.len());
        for message in &messages {
            if message.uid <= lower_bound {
                return Err(InboxValidationError::UidBeforeCursor {
                    uid: message.uid,
                    cursor: lower_bound,
                });
            }
            if !unique.insert(message.uid) {
                return Err(InboxValidationError::DuplicateUid(message.uid));
            }
            if scanned_through_uid.is_none_or(|boundary| boundary < message.uid) {
                return Err(InboxValidationError::ScanBoundaryBeforeMessage {
                    scanned_through_uid,
                    uid: message.uid,
                });
            }
        }
        Ok(Self {
            account_id,
            expected_generation,
            uid_validity,
            progress: InboxReceiveProgress::Forward {
                expected_cursor,
                scanned_through_uid,
                bootstrap_history,
            },
            messages: messages.into_boxed_slice(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_history(
        account_id: AccountId,
        expected_generation: AccountGeneration,
        expected_cursor: u32,
        uid_validity: u32,
        expected_history_cursor: u32,
        next_history_cursor: Option<u32>,
        history_complete: bool,
        messages: Vec<InboxEnvelope>,
    ) -> Result<Self, InboxValidationError> {
        if uid_validity == 0 {
            return Err(InboxValidationError::UidValidity);
        }
        if expected_cursor == 0
            || expected_history_cursor == 0
            || history_complete != next_history_cursor.is_none()
            || next_history_cursor
                .is_some_and(|cursor| cursor == 0 || cursor >= expected_history_cursor)
        {
            return Err(InboxValidationError::HistoryProgress);
        }
        if messages.len() > MAX_RECEIVE_PAGE {
            return Err(InboxValidationError::PageSize {
                found: messages.len(),
                maximum: MAX_RECEIVE_PAGE,
            });
        }
        let lower_bound = next_history_cursor.unwrap_or(0);
        let mut unique = HashSet::with_capacity(messages.len());
        for message in &messages {
            if message.uid <= lower_bound || message.uid > expected_history_cursor {
                return Err(InboxValidationError::HistoryMessageOutsideWindow {
                    uid: message.uid,
                    lower_bound,
                    upper_bound: expected_history_cursor,
                });
            }
            if !unique.insert(message.uid) {
                return Err(InboxValidationError::DuplicateUid(message.uid));
            }
        }
        Ok(Self {
            account_id,
            expected_generation,
            uid_validity,
            progress: InboxReceiveProgress::History {
                expected_cursor,
                expected_history_cursor,
                next_history_cursor,
                history_complete,
            },
            messages: messages.into_boxed_slice(),
        })
    }

    pub(crate) fn account_id(&self) -> AccountId {
        self.account_id
    }

    pub(crate) fn expected_generation(&self) -> AccountGeneration {
        self.expected_generation
    }

    pub(crate) fn expected_cursor(&self) -> Option<u32> {
        match self.progress {
            InboxReceiveProgress::Forward {
                expected_cursor, ..
            } => expected_cursor,
            InboxReceiveProgress::History {
                expected_cursor, ..
            } => Some(expected_cursor),
        }
    }

    pub(crate) fn uid_validity(&self) -> u32 {
        self.uid_validity
    }

    pub(crate) fn scanned_through_uid(&self) -> Option<u32> {
        match self.progress {
            InboxReceiveProgress::Forward {
                scanned_through_uid,
                ..
            } => scanned_through_uid,
            InboxReceiveProgress::History {
                expected_cursor, ..
            } => Some(expected_cursor),
        }
    }

    pub(crate) fn messages(&self) -> &[InboxEnvelope] {
        &self.messages
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StagedInboxMessage {
    pub(crate) uid: u32,
    pub(crate) message_id: MessageId,
    pub(crate) needs_content: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InboxStageOutcome {
    Staged {
        messages: Box<[StagedInboxMessage]>,
        tombstoned: u8,
        ticket: InboxCursorTicket,
    },
    Stale,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InboxCursorTicket {
    account_id: AccountId,
    expected_generation: AccountGeneration,
    uid_validity: u32,
    progress: InboxReceiveProgress,
}

impl InboxCursorTicket {
    pub(crate) fn scanned_through_uid(&self) -> Option<u32> {
        match self.progress {
            InboxReceiveProgress::Forward {
                scanned_through_uid,
                ..
            } => scanned_through_uid,
            InboxReceiveProgress::History {
                expected_cursor, ..
            } => Some(expected_cursor),
        }
    }

    fn cursor_boundary(&self) -> Option<u32> {
        match self.progress {
            InboxReceiveProgress::Forward {
                expected_cursor,
                scanned_through_uid,
                ..
            } => scanned_through_uid.or(expected_cursor),
            InboxReceiveProgress::History {
                expected_cursor, ..
            } => Some(expected_cursor),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InboxCursorCommit {
    ticket: InboxCursorTicket,
    last_sync_at_ms: i64,
}

impl InboxCursorCommit {
    pub(crate) fn new(
        ticket: InboxCursorTicket,
        last_sync_at_ms: i64,
    ) -> Result<Self, InboxValidationError> {
        validate_timestamp(last_sync_at_ms)?;
        Ok(Self {
            ticket,
            last_sync_at_ms,
        })
    }

    pub(crate) fn account_id(&self) -> AccountId {
        self.ticket.account_id
    }

    pub(crate) fn expected_generation(&self) -> AccountGeneration {
        self.ticket.expected_generation
    }

    pub(crate) fn expected_cursor(&self) -> Option<u32> {
        match self.ticket.progress {
            InboxReceiveProgress::Forward {
                expected_cursor, ..
            } => expected_cursor,
            InboxReceiveProgress::History {
                expected_cursor, ..
            } => Some(expected_cursor),
        }
    }

    pub(crate) fn uid_validity(&self) -> u32 {
        self.ticket.uid_validity
    }

    pub(crate) fn scanned_through_uid(&self) -> Option<u32> {
        self.ticket.scanned_through_uid()
    }

    pub(crate) fn last_sync_at_ms(&self) -> i64 {
        self.last_sync_at_ms
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InboxCursorOutcome {
    Committed { scanned_through_uid: Option<u32> },
    Stale,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InboxValidationError {
    Uid,
    UidValidity,
    DuplicateUid(u32),
    UidBeforeCursor {
        uid: u32,
        cursor: u32,
    },
    ScanBoundaryBeforeCursor {
        scanned_through_uid: u32,
        cursor: u32,
    },
    ScanBoundaryBeforeMessage {
        scanned_through_uid: Option<u32>,
        uid: u32,
    },
    HistoryProgress,
    HistoryMessageOutsideWindow {
        uid: u32,
        lower_bound: u32,
        upper_bound: u32,
    },
    PageSize {
        found: usize,
        maximum: usize,
    },
    Encoding {
        field: &'static str,
    },
    TextBytes {
        field: &'static str,
        found: usize,
        maximum: usize,
    },
    Timestamp(i64),
    Flags(u8),
}

pub(super) fn load_inbox_checkpoint(
    connection: &Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<InboxCheckpointOutcome, DbFailure> {
    match active_imap_account_fence(connection, account_id, expected_generation)? {
        ActiveImapAccountFence::Stale => return Ok(InboxCheckpointOutcome::Stale),
        ActiveImapAccountFence::NotFound => return Ok(InboxCheckpointOutcome::NotFound),
        ActiveImapAccountFence::Current => {}
    }

    let stored = connection
        .query_row(
            "SELECT folder.role, state.uid_validity, state.change_cursor,
                    state.history_cursor, state.history_complete
             FROM folders AS folder
             LEFT JOIN sync_state AS state ON state.folder_id = folder.id
             WHERE folder.account_id = ?1 AND folder.remote_key = ?2",
            params![account_id.get(), INBOX_REMOTE_KEY],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((role, stored_uid_validity, stored_cursor, stored_history_cursor, history_complete)) =
        stored
    else {
        return Ok(InboxCheckpointOutcome::Current(InboxCheckpoint::default()));
    };
    if role != "inbox" {
        return Err(DbFailure::conflict(
            "canonical IMAP inbox has a conflicting folder role",
        ));
    }
    let uid_validity = stored_uid_validity
        .map(|value| {
            u32::try_from(value)
                .ok()
                .filter(|value| *value != 0)
                .ok_or_else(|| DbFailure::database("stored IMAP UIDVALIDITY is invalid"))
        })
        .transpose()?;
    let expected_cursor = stored_cursor.as_deref().map(parse_cursor).transpose()?;
    let history_cursor = stored_history_cursor
        .map(|value| {
            u32::try_from(value)
                .ok()
                .filter(|value| *value != 0)
                .ok_or_else(|| DbFailure::database("stored IMAP history cursor is invalid"))
        })
        .transpose()?;
    let history_complete = match history_complete {
        Some(0) => false,
        Some(1) => true,
        Some(_) => return Err(DbFailure::database("stored IMAP history state is invalid")),
        None => false,
    };
    if expected_cursor.is_some() && uid_validity.is_none() {
        return Err(DbFailure::database(
            "stored IMAP cursor has no UIDVALIDITY fence",
        ));
    }
    if history_cursor.is_some() && (expected_cursor.is_none() || uid_validity.is_none()) {
        return Err(DbFailure::database(
            "stored IMAP history cursor has no forward cursor fence",
        ));
    }
    if history_complete && history_cursor.is_some() {
        return Err(DbFailure::database(
            "completed IMAP history scan retains a cursor",
        ));
    }
    Ok(InboxCheckpointOutcome::Current(InboxCheckpoint {
        expected_cursor,
        uid_validity,
        history_cursor,
        history_complete,
    }))
}

pub(super) fn load_imap_message_content_target(
    connection: &Connection,
    message_id: MessageId,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<ImapMessageContentTargetOutcome, DbFailure> {
    match active_imap_account_fence(connection, account_id, expected_generation)? {
        ActiveImapAccountFence::Current => {}
        ActiveImapAccountFence::Stale => return Ok(ImapMessageContentTargetOutcome::Stale),
        ActiveImapAccountFence::NotFound => return Ok(ImapMessageContentTargetOutcome::NotFound),
    }

    let target = connection
        .query_row(
            "SELECT location.uid_validity, location.uid,
                    content.message_id IS NOT NULL
             FROM messages AS message
             JOIN imap_message_locations AS location
               ON location.message_id = message.id
              AND location.account_id = message.account_id
             JOIN folders AS folder
               ON folder.id = location.folder_id
              AND folder.account_id = location.account_id
              AND folder.role = 'inbox'
             JOIN sync_state AS state
               ON state.folder_id = folder.id
              AND state.uid_validity = location.uid_validity
             LEFT JOIN message_content AS content ON content.message_id = message.id
             WHERE message.id = ?1 AND message.account_id = ?2
             LIMIT 1",
            params![message_id.get(), account_id.get()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((uid_validity, uid, content_available)) = target else {
        return Ok(ImapMessageContentTargetOutcome::NotFound);
    };
    if content_available {
        return Ok(ImapMessageContentTargetOutcome::AlreadyAvailable);
    }
    let uid_validity = u32::try_from(uid_validity)
        .ok()
        .filter(|value| *value != 0)
        .ok_or_else(|| DbFailure::database("stored message UIDVALIDITY is invalid"))?;
    let uid = u32::try_from(uid)
        .ok()
        .filter(|value| *value != 0)
        .ok_or_else(|| DbFailure::database("stored message UID is invalid"))?;
    Ok(ImapMessageContentTargetOutcome::Current(
        ImapMessageContentTarget { uid_validity, uid },
    ))
}

pub(super) fn stage_inbox_page(
    connection: &mut Connection,
    page: &InboxReceivePage,
) -> Result<InboxStageOutcome, DbFailure> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    if !account_fence_matches(&transaction, page.account_id, page.expected_generation)? {
        return Ok(InboxStageOutcome::Stale);
    }

    let inbox = load_inbox(&transaction, page.account_id)?;
    let expected_cursor = page.expected_cursor();
    let folder_id = match inbox {
        Some(inbox) => {
            if inbox.role.as_ref() != "inbox" {
                return Err(DbFailure::conflict(
                    "canonical IMAP inbox has a conflicting folder role",
                ));
            }
            if !match_sync_fence(&transaction, inbox.id, expected_cursor, page.uid_validity)? {
                return Ok(InboxStageOutcome::Stale);
            }
            if let InboxReceiveProgress::History {
                expected_history_cursor,
                ..
            } = page.progress
                && !history_fence_matches(&transaction, inbox.id, expected_history_cursor)?
            {
                return Ok(InboxStageOutcome::Stale);
            }
            inbox.id
        }
        None if expected_cursor.is_none()
            && matches!(page.progress, InboxReceiveProgress::Forward { .. }) =>
        {
            let folder_id = create_inbox(&transaction, page.account_id)?;
            if !match_sync_fence(&transaction, folder_id, expected_cursor, page.uid_validity)? {
                return Ok(InboxStageOutcome::Stale);
            }
            folder_id
        }
        None => return Ok(InboxStageOutcome::Stale),
    };
    let outcome = stage_messages(&transaction, folder_id, page)?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(outcome)
}

fn stage_messages(
    transaction: &Transaction<'_>,
    folder_id: i64,
    page: &InboxReceivePage,
) -> Result<InboxStageOutcome, DbFailure> {
    let mut staged = Vec::with_capacity(page.messages.len());
    let mut tombstoned = 0_u8;
    for message in &page.messages {
        if locator_is_tombstoned(transaction, page.account_id, page.uid_validity, message.uid)? {
            tombstoned = tombstoned
                .checked_add(1)
                .ok_or_else(|| DbFailure::resource_limit("inbox tombstone count overflow"))?;
            continue;
        }
        let message_id = upsert_message(
            transaction,
            folder_id,
            page.account_id,
            page.uid_validity,
            message,
        )?;
        let needs_content = !message_has_content(transaction, message_id)?;
        staged.push(StagedInboxMessage {
            uid: message.uid,
            message_id,
            needs_content,
        });
    }
    match page.progress {
        InboxReceiveProgress::Forward {
            expected_cursor, ..
        } => enforce_pending_window_bound(
            transaction,
            folder_id,
            page.uid_validity,
            expected_cursor.unwrap_or(0),
        )?,
        InboxReceiveProgress::History {
            expected_history_cursor,
            next_history_cursor,
            ..
        } => enforce_history_window_bound(
            transaction,
            folder_id,
            page.uid_validity,
            next_history_cursor.unwrap_or(0),
            expected_history_cursor,
        )?,
    }
    stats::rebuild_account(transaction, page.account_id.get())?;
    Ok(InboxStageOutcome::Staged {
        messages: staged.into_boxed_slice(),
        tombstoned,
        ticket: InboxCursorTicket {
            account_id: page.account_id,
            expected_generation: page.expected_generation,
            uid_validity: page.uid_validity,
            progress: page.progress,
        },
    })
}

pub(super) fn commit_inbox_cursor(
    connection: &mut Connection,
    commit: &InboxCursorCommit,
) -> Result<InboxCursorOutcome, DbFailure> {
    let cursor_boundary = commit.ticket.cursor_boundary();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    if !account_fence_matches(
        &transaction,
        commit.ticket.account_id,
        commit.ticket.expected_generation,
    )? {
        return Ok(InboxCursorOutcome::Stale);
    }
    let Some(inbox) = load_inbox(&transaction, commit.ticket.account_id)? else {
        return Ok(InboxCursorOutcome::Stale);
    };
    let expected_cursor = match commit.ticket.progress {
        InboxReceiveProgress::Forward {
            expected_cursor, ..
        } => expected_cursor,
        InboxReceiveProgress::History {
            expected_cursor, ..
        } => Some(expected_cursor),
    };
    if inbox.role.as_ref() != "inbox"
        || !match_sync_fence(
            &transaction,
            inbox.id,
            expected_cursor,
            commit.ticket.uid_validity,
        )?
    {
        return Ok(InboxCursorOutcome::Stale);
    }

    if let InboxReceiveProgress::History {
        expected_history_cursor,
        ..
    } = commit.ticket.progress
        && !history_fence_matches(&transaction, inbox.id, expected_history_cursor)?
    {
        return Ok(InboxCursorOutcome::Stale);
    }
    let changed = match commit.ticket.progress {
        InboxReceiveProgress::Forward {
            expected_cursor,
            bootstrap_history: Some(history),
            ..
        } => transaction.execute(
            "UPDATE sync_state
             SET change_cursor = ?2, last_sync_at_ms = ?3,
                 history_cursor = ?6, history_complete = ?7
             WHERE folder_id = ?1 AND uid_validity = ?4
               AND ((?5 IS NULL AND change_cursor IS NULL) OR change_cursor = ?5)",
            params![
                inbox.id,
                cursor_boundary.map(|uid| uid.to_string()),
                commit.last_sync_at_ms,
                i64::from(commit.ticket.uid_validity),
                expected_cursor.map(|cursor| cursor.to_string()),
                history.cursor.map(i64::from),
                i64::from(history.complete),
            ],
        ),
        InboxReceiveProgress::Forward {
            expected_cursor,
            bootstrap_history: None,
            ..
        } => transaction.execute(
            "UPDATE sync_state
             SET change_cursor = ?2, last_sync_at_ms = ?3
             WHERE folder_id = ?1 AND uid_validity = ?4
               AND ((?5 IS NULL AND change_cursor IS NULL) OR change_cursor = ?5)",
            params![
                inbox.id,
                cursor_boundary.map(|uid| uid.to_string()),
                commit.last_sync_at_ms,
                i64::from(commit.ticket.uid_validity),
                expected_cursor.map(|cursor| cursor.to_string()),
            ],
        ),
        InboxReceiveProgress::History {
            expected_cursor,
            expected_history_cursor,
            next_history_cursor,
            history_complete,
        } => transaction.execute(
            "UPDATE sync_state
             SET history_cursor = ?2, history_complete = ?3, last_sync_at_ms = ?4
             WHERE folder_id = ?1 AND uid_validity = ?5
               AND change_cursor = ?6
               AND history_cursor = ?7 AND history_complete = 0",
            params![
                inbox.id,
                next_history_cursor.map(i64::from),
                i64::from(history_complete),
                commit.last_sync_at_ms,
                i64::from(commit.ticket.uid_validity),
                expected_cursor.to_string(),
                i64::from(expected_history_cursor),
            ],
        ),
    }
    .map_err(DbFailure::database)?;
    if changed != 1 {
        return Ok(InboxCursorOutcome::Stale);
    }
    transaction.commit().map_err(DbFailure::database)?;
    Ok(InboxCursorOutcome::Committed {
        scanned_through_uid: cursor_boundary,
    })
}

#[derive(Clone, Debug)]
struct InboxFolder {
    id: i64,
    role: Box<str>,
}

fn account_fence_matches(
    connection: &Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<bool, DbFailure> {
    active_imap_account_fence(connection, account_id, expected_generation)
        .map(|outcome| outcome == ActiveImapAccountFence::Current)
}

pub(super) fn active_imap_account_fence(
    connection: &Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<ActiveImapAccountFence, DbFailure> {
    let account = connection
        .query_row(
            "SELECT account.configuration_generation, account.provider, account.state,
                    EXISTS (
                        SELECT 1 FROM account_connections AS configured
                        WHERE configured.account_id = account.id
                    )
             FROM accounts AS account
             WHERE account.id = ?1",
            [account_id.get()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            },
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((generation, provider, state, configured)) = account else {
        return Ok(ActiveImapAccountFence::NotFound);
    };
    if generation == expected_generation.get()
        && provider == "imap"
        && state == "active"
        && configured
    {
        Ok(ActiveImapAccountFence::Current)
    } else {
        Ok(ActiveImapAccountFence::Stale)
    }
}

fn load_inbox(
    connection: &Connection,
    account_id: AccountId,
) -> Result<Option<InboxFolder>, DbFailure> {
    let existing = connection
        .query_row(
            "SELECT id, role FROM folders
             WHERE account_id = ?1 AND remote_key = ?2",
            params![account_id.get(), INBOX_REMOTE_KEY],
            |row| {
                Ok(InboxFolder {
                    id: row.get(0)?,
                    role: row.get::<_, String>(1)?.into_boxed_str(),
                })
            },
        )
        .optional()
        .map_err(DbFailure::database)?;
    Ok(existing)
}

fn create_inbox(transaction: &Transaction<'_>, account_id: AccountId) -> Result<i64, DbFailure> {
    transaction
        .execute(
            "INSERT INTO folders (account_id, remote_key, name, role)
             VALUES (?1, ?2, 'Inbox', 'inbox')",
            params![account_id.get(), INBOX_REMOTE_KEY],
        )
        .map_err(DbFailure::database)?;
    let folder_id = transaction.last_insert_rowid();
    if folder_id <= 0 {
        return Err(DbFailure::database("invalid canonical inbox identity"));
    }
    Ok(folder_id)
}

fn match_sync_fence(
    transaction: &Transaction<'_>,
    folder_id: i64,
    expected_cursor: Option<u32>,
    uid_validity: u32,
) -> Result<bool, DbFailure> {
    let state = transaction
        .query_row(
            "SELECT uid_validity, change_cursor FROM sync_state WHERE folder_id = ?1",
            [folder_id],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()
        .map_err(DbFailure::database)?;
    match state {
        None if expected_cursor.is_none() => {
            transaction
                .execute(
                    "INSERT INTO sync_state (folder_id, uid_validity)
                     VALUES (?1, ?2)",
                    params![folder_id, i64::from(uid_validity)],
                )
                .map_err(DbFailure::database)?;
            Ok(true)
        }
        None => Ok(false),
        Some((stored_uid_validity, stored_cursor)) => {
            let cursor = stored_cursor.as_deref().map(parse_cursor).transpose()?;
            if cursor != expected_cursor {
                return Ok(false);
            }
            match stored_uid_validity {
                Some(stored) => Ok(stored == i64::from(uid_validity)),
                None if expected_cursor.is_none() => {
                    transaction
                        .execute(
                            "UPDATE sync_state SET uid_validity = ?2
                             WHERE folder_id = ?1 AND uid_validity IS NULL
                               AND change_cursor IS NULL",
                            params![folder_id, i64::from(uid_validity)],
                        )
                        .map_err(DbFailure::database)?;
                    Ok(true)
                }
                None => Ok(false),
            }
        }
    }
}

fn history_fence_matches(
    transaction: &Transaction<'_>,
    folder_id: i64,
    expected_history_cursor: u32,
) -> Result<bool, DbFailure> {
    transaction
        .query_row(
            "SELECT history_cursor = ?2 AND history_complete = 0
             FROM sync_state WHERE folder_id = ?1",
            params![folder_id, i64::from(expected_history_cursor)],
            |row| row.get(0),
        )
        .optional()
        .map(|matched| matched.unwrap_or(false))
        .map_err(DbFailure::database)
}

fn locator_is_tombstoned(
    transaction: &Transaction<'_>,
    account_id: AccountId,
    uid_validity: u32,
    uid: u32,
) -> Result<bool, DbFailure> {
    let remote_key = message_remote_key(uid_validity, uid);
    transaction
        .query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM message_tombstone_imap_locations
                 WHERE account_id = ?1 AND folder_key = ?2
                   AND uid_validity = ?3 AND uid = ?4
             ) OR EXISTS (
                 SELECT 1 FROM message_tombstones
                 WHERE account_id = ?1 AND remote_key = ?5
             )",
            params![
                account_id.get(),
                INBOX_REMOTE_KEY,
                i64::from(uid_validity),
                i64::from(uid),
                remote_key,
            ],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
}

fn upsert_message(
    transaction: &Transaction<'_>,
    folder_id: i64,
    account_id: AccountId,
    uid_validity: u32,
    message: &InboxEnvelope,
) -> Result<MessageId, DbFailure> {
    let existing_locator = transaction
        .query_row(
            "SELECT message_id FROM imap_message_locations
             WHERE folder_id = ?1 AND uid_validity = ?2 AND uid = ?3",
            params![folder_id, i64::from(uid_validity), i64::from(message.uid)],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(DbFailure::database)?;
    let remote_key = message_remote_key(uid_validity, message.uid);
    let message_id = match existing_locator {
        Some(id) => {
            let changed = update_envelope(transaction, id, account_id, message)?;
            if changed != 1 {
                return Err(DbFailure::conflict(
                    "IMAP locator points to a missing or foreign message",
                ));
            }
            MessageId::new(id)
                .map_err(|_| DbFailure::database("invalid staged message identity"))?
        }
        None => {
            let existing = transaction
                .query_row(
                    "SELECT id FROM messages WHERE account_id = ?1 AND remote_key = ?2",
                    params![account_id.get(), remote_key],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(DbFailure::database)?;
            if let Some(id) = existing {
                let changed = update_envelope(transaction, id, account_id, message)?;
                if changed != 1 {
                    return Err(DbFailure::conflict(
                        "staged IMAP message changed during import",
                    ));
                }
                MessageId::new(id)
                    .map_err(|_| DbFailure::database("invalid staged message identity"))?
            } else {
                transaction
                    .execute(
                        "INSERT INTO messages
                         (account_id, remote_key, sender_name, sender_address, subject, preview,
                          received_at_ms, unread, starred, has_attachment)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                        params![
                            account_id.get(),
                            remote_key,
                            message.sender_name.as_ref(),
                            message.sender_address.as_ref(),
                            message.subject.as_ref(),
                            message.preview.as_ref(),
                            message.received_at_ms,
                            !message.flags.seen(),
                            message.flags.flagged(),
                            message.has_attachment,
                        ],
                    )
                    .map_err(DbFailure::database)?;
                MessageId::new(transaction.last_insert_rowid())
                    .map_err(|_| DbFailure::database("invalid staged message identity"))?
            }
        }
    };

    transaction
        .execute(
            "INSERT OR IGNORE INTO message_folders (message_id, folder_id, account_id)
             VALUES (?1, ?2, ?3)",
            params![message_id.get(), folder_id, account_id.get()],
        )
        .map_err(DbFailure::database)?;
    if existing_locator.is_some() {
        transaction
            .execute(
                "UPDATE imap_message_locations
                 SET remote_seen = ?4, remote_flagged = ?5
                 WHERE message_id = ?1 AND folder_id = ?2 AND account_id = ?3",
                params![
                    message_id.get(),
                    folder_id,
                    account_id.get(),
                    message.flags.seen(),
                    message.flags.flagged(),
                ],
            )
            .map_err(DbFailure::database)?;
    } else {
        transaction
            .execute(
                "INSERT INTO imap_message_locations
                 (message_id, folder_id, account_id, uid_validity, uid,
                  remote_seen, remote_flagged)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    message_id.get(),
                    folder_id,
                    account_id.get(),
                    i64::from(uid_validity),
                    i64::from(message.uid),
                    message.flags.seen(),
                    message.flags.flagged(),
                ],
            )
            .map_err(DbFailure::database)?;
    }
    Ok(message_id)
}

fn update_envelope(
    transaction: &Transaction<'_>,
    message_id: i64,
    account_id: AccountId,
    message: &InboxEnvelope,
) -> Result<usize, DbFailure> {
    transaction
        .execute(
            "UPDATE messages
             SET sender_name = CASE WHEN EXISTS (
                         SELECT 1 FROM message_content WHERE message_id = ?1
                     ) THEN sender_name ELSE ?3 END,
                 sender_address = CASE WHEN EXISTS (
                         SELECT 1 FROM message_content WHERE message_id = ?1
                     ) THEN sender_address ELSE ?4 END,
                 subject = CASE WHEN EXISTS (
                         SELECT 1 FROM message_content WHERE message_id = ?1
                     ) THEN subject ELSE ?5 END,
                 preview = CASE WHEN EXISTS (
                         SELECT 1 FROM message_content WHERE message_id = ?1
                     ) THEN preview ELSE ?6 END,
                 received_at_ms = CASE WHEN EXISTS (
                         SELECT 1 FROM message_content WHERE message_id = ?1
                     ) THEN received_at_ms ELSE ?7 END,
                 has_attachment = CASE WHEN EXISTS (
                         SELECT 1 FROM message_content WHERE message_id = ?1
                     ) THEN has_attachment ELSE ?8 END
             WHERE id = ?1 AND account_id = ?2",
            params![
                message_id,
                account_id.get(),
                message.sender_name.as_ref(),
                message.sender_address.as_ref(),
                message.subject.as_ref(),
                message.preview.as_ref(),
                message.received_at_ms,
                message.has_attachment,
            ],
        )
        .map_err(DbFailure::database)
}

fn message_has_content(
    transaction: &Transaction<'_>,
    message_id: MessageId,
) -> Result<bool, DbFailure> {
    transaction
        .query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM message_content WHERE message_id = ?1
             )",
            [message_id.get()],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
}

fn enforce_pending_window_bound(
    transaction: &Transaction<'_>,
    folder_id: i64,
    uid_validity: u32,
    cursor: u32,
) -> Result<(), DbFailure> {
    let count = transaction
        .prepare(
            "SELECT uid FROM imap_message_locations
             WHERE folder_id = ?1 AND uid_validity = ?2 AND uid > ?3
             ORDER BY uid LIMIT ?4",
        )
        .and_then(|mut statement| {
            let rows = statement.query_map(
                params![
                    folder_id,
                    i64::from(uid_validity),
                    i64::from(cursor),
                    i64::try_from(MAX_RECEIVE_PAGE + 1).expect("page bound fits i64"),
                ],
                |row| row.get::<_, i64>(0),
            )?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .map_err(DbFailure::database)?
        .len();
    if count > MAX_RECEIVE_PAGE {
        return Err(DbFailure::resource_limit(
            "pending inbox receive window exceeds the page limit",
        ));
    }
    Ok(())
}

fn enforce_history_window_bound(
    transaction: &Transaction<'_>,
    folder_id: i64,
    uid_validity: u32,
    lower_bound: u32,
    upper_bound: u32,
) -> Result<(), DbFailure> {
    let count = transaction
        .query_row(
            "SELECT count(*) FROM (
                 SELECT 1 FROM imap_message_locations
                 WHERE folder_id = ?1 AND uid_validity = ?2
                   AND uid > ?3 AND uid <= ?4
                 LIMIT ?5
             )",
            params![
                folder_id,
                i64::from(uid_validity),
                i64::from(lower_bound),
                i64::from(upper_bound),
                i64::try_from(MAX_RECEIVE_PAGE + 1).expect("page bound fits i64"),
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(DbFailure::database)?;
    if count > i64::try_from(MAX_RECEIVE_PAGE).expect("receive page limit fits i64") {
        return Err(DbFailure::resource_limit(
            "inbox history window exceeds the receive page limit",
        ));
    }
    Ok(())
}

fn parse_cursor(value: &str) -> Result<u32, DbFailure> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_| DbFailure::conflict("stored IMAP cursor is not a decimal UID"))?;
    if parsed.to_string() != value {
        return Err(DbFailure::conflict(
            "stored IMAP cursor is not canonically encoded",
        ));
    }
    Ok(parsed)
}

fn message_remote_key(uid_validity: u32, uid: u32) -> String {
    format!("imap:inbox:{uid_validity}:{uid}")
}

fn validate_text(
    field: &'static str,
    bytes: &[u8],
    maximum: usize,
) -> Result<Box<str>, InboxValidationError> {
    if bytes.len() > maximum {
        return Err(InboxValidationError::TextBytes {
            field,
            found: bytes.len(),
            maximum,
        });
    }
    str::from_utf8(bytes)
        .map(Box::<str>::from)
        .map_err(|_| InboxValidationError::Encoding { field })
}

fn validate_timestamp(value: i64) -> Result<(), InboxValidationError> {
    if !(MIN_TIMESTAMP_MS..=MAX_TIMESTAMP_MS).contains(&value) {
        return Err(InboxValidationError::Timestamp(value));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sqlite::{
        domain::{AccountScope, FolderScope, PageBoundary, PageSpec},
        migrations::migrate,
        query::query_mailbox,
    };

    const GENERATION: i64 = 7;
    const UID_VALIDITY: u32 = 19;

    fn connection() -> Connection {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        connection
            .execute_batch(&format!(
                "INSERT INTO accounts
                 (id, provider, remote_key, name, address, state, accent_rgb,
                  configuration_generation)
                 VALUES (1, 'imap', 'account', 'Personal', 'user@example.test',
                         'active', 1, {GENERATION});
                 INSERT INTO account_connections
                 (account_id, credential_key, auth_kind, login_name, imap_host, imap_port,
                  diagnostic_generation, diagnostic_state, last_checked_at_ms)
                 VALUES (1, '00000000000000000000000000000001', 'app_password',
                         'user@example.test', 'imap.example.test', 993, 1, 'ready', 1000);"
            ))
            .unwrap();
        connection
    }

    fn account_id() -> AccountId {
        AccountId::new(1).unwrap()
    }

    fn generation(value: i64) -> AccountGeneration {
        AccountGeneration::new(value).unwrap()
    }

    fn envelope(uid: u32, subject: &str, flags: InboxFlags) -> InboxEnvelope {
        InboxEnvelope::new(
            uid,
            b"Ada",
            b"ada@example.test",
            subject.as_bytes(),
            b"A bounded preview",
            1_700_000_000_000 + i64::from(uid),
            flags,
            false,
        )
        .unwrap()
    }

    fn page(
        expected_cursor: Option<u32>,
        uid_validity: u32,
        messages: Vec<InboxEnvelope>,
    ) -> InboxReceivePage {
        let scanned_through_uid = messages
            .iter()
            .map(InboxEnvelope::uid)
            .max()
            .or(expected_cursor);
        scanned_page(expected_cursor, uid_validity, scanned_through_uid, messages)
    }

    fn scanned_page(
        expected_cursor: Option<u32>,
        uid_validity: u32,
        scanned_through_uid: Option<u32>,
        messages: Vec<InboxEnvelope>,
    ) -> InboxReceivePage {
        InboxReceivePage::new(
            account_id(),
            generation(GENERATION),
            expected_cursor,
            uid_validity,
            scanned_through_uid,
            messages,
        )
        .unwrap()
    }

    fn stage_parts(outcome: InboxStageOutcome) -> (Box<[StagedInboxMessage]>, InboxCursorTicket) {
        match outcome {
            InboxStageOutcome::Staged {
                messages, ticket, ..
            } => (messages, ticket),
            InboxStageOutcome::Stale => panic!("stage unexpectedly stale"),
        }
    }

    fn add_content(connection: &Connection, messages: &[StagedInboxMessage]) {
        for message in messages {
            connection
                .execute(
                    "INSERT INTO message_content
                     (message_id, reader_excerpt, truncated, body_byte_count)
                     VALUES (?1, 'Body', 0, 4)",
                    [message.message_id.get()],
                )
                .unwrap();
        }
    }

    fn sync_state(connection: &Connection) -> Option<(i64, Option<String>, Option<i64>)> {
        connection
            .query_row(
                "SELECT uid_validity, change_cursor, last_sync_at_ms FROM sync_state",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .unwrap()
    }

    fn history_state(connection: &Connection) -> (Option<String>, Option<i64>, bool) {
        connection
            .query_row(
                "SELECT change_cursor, history_cursor, history_complete FROM sync_state",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    fn stage_content_and_commit(
        connection: &mut Connection,
        page: &InboxReceivePage,
        timestamp: i64,
    ) -> InboxCursorOutcome {
        let (messages, ticket) = stage_parts(stage_inbox_page(connection, page).unwrap());
        add_content(connection, &messages);
        commit_inbox_cursor(
            connection,
            &InboxCursorCommit::new(ticket, timestamp).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn recent_bootstrap_and_history_pages_commit_independent_bounded_cursors() {
        let mut connection = connection();
        let bootstrap = InboxReceivePage::new_bootstrap(
            account_id(),
            generation(GENERATION),
            UID_VALIDITY,
            Some(6),
            Some(4),
            false,
            vec![
                envelope(5, "Five", InboxFlags::new(false, false)),
                envelope(6, "Six", InboxFlags::new(false, false)),
            ],
        )
        .unwrap();
        assert_eq!(
            stage_content_and_commit(&mut connection, &bootstrap, 2_000),
            InboxCursorOutcome::Committed {
                scanned_through_uid: Some(6)
            }
        );
        assert_eq!(
            history_state(&connection),
            (Some("6".into()), Some(4), false)
        );

        let middle = InboxReceivePage::new_history(
            account_id(),
            generation(GENERATION),
            6,
            UID_VALIDITY,
            4,
            Some(2),
            false,
            vec![
                envelope(4, "Four", InboxFlags::new(false, false)),
                envelope(3, "Three", InboxFlags::new(false, false)),
            ],
        )
        .unwrap();
        assert_eq!(
            stage_content_and_commit(&mut connection, &middle, 3_000),
            InboxCursorOutcome::Committed {
                scanned_through_uid: Some(6)
            }
        );
        assert_eq!(
            history_state(&connection),
            (Some("6".into()), Some(2), false)
        );

        let oldest = InboxReceivePage::new_history(
            account_id(),
            generation(GENERATION),
            6,
            UID_VALIDITY,
            2,
            None,
            true,
            vec![
                envelope(2, "Two", InboxFlags::new(false, false)),
                envelope(1, "One", InboxFlags::new(false, false)),
            ],
        )
        .unwrap();
        assert_eq!(
            stage_content_and_commit(&mut connection, &oldest, 4_000),
            InboxCursorOutcome::Committed {
                scanned_through_uid: Some(6)
            }
        );
        assert_eq!(history_state(&connection), (Some("6".into()), None, true));
        assert_eq!(
            load_inbox_checkpoint(&connection, account_id(), generation(GENERATION)).unwrap(),
            InboxCheckpointOutcome::Current(InboxCheckpoint {
                expected_cursor: Some(6),
                uid_validity: Some(UID_VALIDITY),
                history_cursor: None,
                history_complete: true,
            })
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            6
        );
    }

    #[test]
    fn validates_page_identity_text_time_and_flags() {
        assert_eq!(
            InboxFlags::from_bits(4),
            Err(InboxValidationError::Flags(4))
        );
        assert_eq!(
            InboxEnvelope::new(
                0,
                b"",
                b"",
                b"",
                b"",
                0,
                InboxFlags::new(false, false),
                false,
            ),
            Err(InboxValidationError::Uid)
        );
        assert!(matches!(
            InboxEnvelope::new(
                1,
                &[0xff],
                b"",
                b"",
                b"",
                0,
                InboxFlags::new(false, false),
                false,
            ),
            Err(InboxValidationError::Encoding {
                field: "sender name"
            })
        ));
        assert!(matches!(
            InboxEnvelope::new(
                1,
                b"",
                b"",
                &vec![b'x'; MAX_SUBJECT_BYTES + 1],
                b"",
                0,
                InboxFlags::new(false, false),
                false,
            ),
            Err(InboxValidationError::TextBytes {
                field: "subject",
                ..
            })
        ));
        assert!(matches!(
            InboxEnvelope::new(
                1,
                b"",
                b"",
                b"",
                b"",
                i64::MAX,
                InboxFlags::new(false, false),
                false,
            ),
            Err(InboxValidationError::Timestamp(i64::MAX))
        ));
        assert!(matches!(
            InboxReceivePage::new(
                account_id(),
                generation(GENERATION),
                None,
                0,
                None,
                Vec::new(),
            ),
            Err(InboxValidationError::UidValidity)
        ));
        assert_eq!(
            InboxReceivePage::new(
                account_id(),
                generation(GENERATION),
                Some(8),
                UID_VALIDITY,
                Some(7),
                Vec::new(),
            ),
            Err(InboxValidationError::ScanBoundaryBeforeCursor {
                scanned_through_uid: 7,
                cursor: 8,
            })
        );
        assert!(matches!(
            InboxReceivePage::new(
                account_id(),
                generation(GENERATION),
                None,
                UID_VALIDITY,
                Some(4),
                vec![envelope(5, "Beyond scan", InboxFlags::new(false, false))],
            ),
            Err(InboxValidationError::ScanBoundaryBeforeMessage {
                scanned_through_uid: Some(4),
                uid: 5,
            })
        ));
        assert!(matches!(
            InboxReceivePage::new(
                account_id(),
                generation(GENERATION),
                None,
                UID_VALIDITY,
                Some(1),
                vec![
                    envelope(1, "One", InboxFlags::new(false, false)),
                    envelope(1, "Again", InboxFlags::new(false, false)),
                ],
            ),
            Err(InboxValidationError::DuplicateUid(1))
        ));
        let oversized = (1..=51)
            .map(|uid| envelope(uid, "Mail", InboxFlags::new(false, false)))
            .collect();
        assert!(matches!(
            InboxReceivePage::new(
                account_id(),
                generation(GENERATION),
                None,
                UID_VALIDITY,
                Some(51),
                oversized,
            ),
            Err(InboxValidationError::PageSize {
                found: 51,
                maximum: 50
            })
        ));
        assert_eq!(
            InboxReceivePage::new_bootstrap(
                account_id(),
                generation(GENERATION),
                UID_VALIDITY,
                Some(1),
                Some(0),
                false,
                Vec::new(),
            ),
            Err(InboxValidationError::HistoryProgress)
        );
        assert_eq!(
            InboxReceivePage::new_history(
                account_id(),
                generation(GENERATION),
                1,
                UID_VALIDITY,
                1,
                Some(0),
                false,
                Vec::new(),
            ),
            Err(InboxValidationError::HistoryProgress)
        );
    }

    #[test]
    fn stages_visible_mail_and_commits_cursor_before_content_import() {
        let mut connection = connection();
        let outcome = stage_inbox_page(
            &mut connection,
            &page(
                None,
                UID_VALIDITY,
                vec![
                    envelope(41, "First", InboxFlags::new(false, true)),
                    envelope(42, "Second", InboxFlags::new(true, false)),
                ],
            ),
        )
        .unwrap();
        let (staged, ticket) = stage_parts(outcome);
        assert_eq!(staged.len(), 2);

        let spec = PageSpec::new(
            AccountScope::Account(account_id()),
            FolderScope::Inbox,
            None,
            PageBoundary::First,
            16,
        )
        .unwrap();
        let mailbox = query_mailbox(&connection, &spec).unwrap();
        assert_eq!(mailbox.rows.len(), 2);
        assert_eq!(mailbox.stats.selected_total, Some(2));
        assert_eq!(mailbox.stats.inbox_unread, 1);
        assert_eq!(sync_state(&connection), Some((19, None, None)));

        let commit = InboxCursorCommit::new(ticket, 2_000).unwrap();
        assert_eq!(
            commit_inbox_cursor(&mut connection, &commit).unwrap(),
            InboxCursorOutcome::Committed {
                scanned_through_uid: Some(42)
            }
        );
        assert_eq!(
            sync_state(&connection),
            Some((19, Some("42".to_owned()), Some(2_000)))
        );
        assert!(staged.iter().all(|message| message.needs_content));
    }

    #[test]
    fn on_demand_content_target_is_fenced_and_disappears_after_import() {
        let mut connection = connection();
        let (staged, ticket) = stage_parts(
            stage_inbox_page(
                &mut connection,
                &page(
                    None,
                    UID_VALIDITY,
                    vec![envelope(9, "Selected", InboxFlags::new(false, false))],
                ),
            )
            .unwrap(),
        );
        assert_eq!(
            commit_inbox_cursor(
                &mut connection,
                &InboxCursorCommit::new(ticket, 2_000).unwrap(),
            )
            .unwrap(),
            InboxCursorOutcome::Committed {
                scanned_through_uid: Some(9)
            }
        );
        assert_eq!(
            load_imap_message_content_target(
                &connection,
                staged[0].message_id,
                account_id(),
                generation(GENERATION),
            )
            .unwrap(),
            ImapMessageContentTargetOutcome::Current(ImapMessageContentTarget {
                uid_validity: UID_VALIDITY,
                uid: 9,
            })
        );
        add_content(&connection, &staged);
        assert_eq!(
            load_imap_message_content_target(
                &connection,
                staged[0].message_id,
                account_id(),
                generation(GENERATION),
            )
            .unwrap(),
            ImapMessageContentTargetOutcome::AlreadyAvailable
        );
        assert_eq!(
            load_imap_message_content_target(
                &connection,
                staged[0].message_id,
                account_id(),
                generation(GENERATION + 1),
            )
            .unwrap(),
            ImapMessageContentTargetOutcome::Stale
        );
    }

    #[test]
    fn empty_page_ticket_updates_sync_time_without_advancing_cursor() {
        let mut connection = connection();
        let (messages, ticket) = stage_parts(
            stage_inbox_page(&mut connection, &page(None, UID_VALIDITY, Vec::new())).unwrap(),
        );
        assert!(messages.is_empty());
        assert_eq!(ticket.scanned_through_uid(), None);

        let commit = InboxCursorCommit::new(ticket, 3_000).unwrap();
        assert_eq!(
            commit_inbox_cursor(&mut connection, &commit).unwrap(),
            InboxCursorOutcome::Committed {
                scanned_through_uid: None
            }
        );
        assert_eq!(sync_state(&connection), Some((19, None, Some(3_000))));
    }

    #[test]
    fn empty_scanned_ranges_advance_initial_existing_and_max_cursors() {
        let mut connection = connection();
        for (expected_cursor, scanned_through_uid, committed_cursor, timestamp) in [
            (None, Some(7), Some(7), 3_100),
            (Some(7), None, Some(7), 3_200),
            (Some(7), Some(99), Some(99), 3_300),
            (Some(99), Some(u32::MAX), Some(u32::MAX), 3_400),
        ] {
            let (messages, ticket) = stage_parts(
                stage_inbox_page(
                    &mut connection,
                    &scanned_page(
                        expected_cursor,
                        UID_VALIDITY,
                        scanned_through_uid,
                        Vec::new(),
                    ),
                )
                .unwrap(),
            );
            assert!(messages.is_empty());
            assert_eq!(ticket.scanned_through_uid(), scanned_through_uid);

            let commit = InboxCursorCommit::new(ticket, timestamp).unwrap();
            assert_eq!(
                commit_inbox_cursor(&mut connection, &commit).unwrap(),
                InboxCursorOutcome::Committed {
                    scanned_through_uid: committed_cursor,
                }
            );
            assert_eq!(
                sync_state(&connection),
                Some((
                    i64::from(UID_VALIDITY),
                    committed_cursor.map(|cursor| cursor.to_string()),
                    Some(timestamp),
                ))
            );
        }
    }

    #[test]
    fn committing_one_scan_ticket_makes_an_older_ticket_stale() {
        let mut connection = connection();
        let (_, older_ticket) = stage_parts(
            stage_inbox_page(
                &mut connection,
                &scanned_page(None, UID_VALIDITY, Some(12), Vec::new()),
            )
            .unwrap(),
        );
        let (_, winning_ticket) = stage_parts(
            stage_inbox_page(
                &mut connection,
                &scanned_page(None, UID_VALIDITY, Some(12), Vec::new()),
            )
            .unwrap(),
        );

        let winner = InboxCursorCommit::new(winning_ticket, 3_500).unwrap();
        assert_eq!(
            commit_inbox_cursor(&mut connection, &winner).unwrap(),
            InboxCursorOutcome::Committed {
                scanned_through_uid: Some(12),
            }
        );
        let before = sync_state(&connection);
        let stale = InboxCursorCommit::new(older_ticket, 3_600).unwrap();
        assert_eq!(
            commit_inbox_cursor(&mut connection, &stale).unwrap(),
            InboxCursorOutcome::Stale
        );
        assert_eq!(sync_state(&connection), before);
    }

    #[test]
    fn restaging_is_idempotent_and_preserves_local_desired_flags() {
        let mut connection = connection();
        let (first, _) = stage_parts(
            stage_inbox_page(
                &mut connection,
                &page(
                    None,
                    UID_VALIDITY,
                    vec![envelope(7, "Old", InboxFlags::new(false, false))],
                ),
            )
            .unwrap(),
        );
        connection
            .execute(
                "UPDATE messages SET unread = 0, starred = 1 WHERE id = ?1",
                [first[0].message_id.get()],
            )
            .unwrap();

        let (second, _) = stage_parts(
            stage_inbox_page(
                &mut connection,
                &page(
                    None,
                    UID_VALIDITY,
                    vec![envelope(7, "Updated", InboxFlags::new(false, false))],
                ),
            )
            .unwrap(),
        );
        assert_eq!(first, second);
        let stored: (i64, bool, bool, String, bool, bool) = connection
            .query_row(
                "SELECT count(*), message.unread, message.starred, message.subject,
                        location.remote_seen, location.remote_flagged
                 FROM messages AS message
                 JOIN imap_message_locations AS location ON location.message_id = message.id",
                [],
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
            .unwrap();
        assert_eq!(stored, (1, false, true, "Updated".to_owned(), false, false));
    }

    #[test]
    fn restaging_complete_content_skips_reimport_and_preserves_mime_metadata() {
        let mut connection = connection();
        let (first, _) = stage_parts(
            stage_inbox_page(
                &mut connection,
                &page(
                    None,
                    UID_VALIDITY,
                    vec![envelope(
                        8,
                        "Envelope subject",
                        InboxFlags::new(false, false),
                    )],
                ),
            )
            .unwrap(),
        );
        assert!(first[0].needs_content);
        add_content(&connection, &first);
        connection
            .execute(
                "UPDATE messages
                 SET subject = 'MIME subject', preview = 'MIME preview', has_attachment = 1
                 WHERE id = ?1",
                [first[0].message_id.get()],
            )
            .unwrap();

        let (second, _) = stage_parts(
            stage_inbox_page(
                &mut connection,
                &page(
                    None,
                    UID_VALIDITY,
                    vec![envelope(8, "Changed envelope", InboxFlags::new(true, true))],
                ),
            )
            .unwrap(),
        );
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].message_id, first[0].message_id);
        assert!(!second[0].needs_content);
        let stored: (String, String, bool, bool, bool) = connection
            .query_row(
                "SELECT message.subject, message.preview, message.has_attachment,
                        location.remote_seen, location.remote_flagged
                 FROM messages AS message
                 JOIN imap_message_locations AS location ON location.message_id = message.id
                 WHERE message.id = ?1",
                [first[0].message_id.get()],
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
                "MIME subject".to_owned(),
                "MIME preview".to_owned(),
                true,
                true,
                true,
            )
        );
    }

    #[test]
    fn stage_fences_generation_cursor_uidvalidity_and_tombstones() {
        let mut connection = connection();
        let stale_generation = InboxReceivePage::new(
            account_id(),
            generation(GENERATION - 1),
            None,
            UID_VALIDITY,
            Some(1),
            vec![envelope(1, "Stale", InboxFlags::new(false, false))],
        )
        .unwrap();
        assert_eq!(
            stage_inbox_page(&mut connection, &stale_generation).unwrap(),
            InboxStageOutcome::Stale
        );
        assert_eq!(sync_state(&connection), None);

        stage_inbox_page(
            &mut connection,
            &page(
                None,
                UID_VALIDITY,
                vec![envelope(2, "Current", InboxFlags::new(false, false))],
            ),
        )
        .unwrap();
        let before: (i64, String) = connection
            .query_row("SELECT count(*), subject FROM messages", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(
            stage_inbox_page(
                &mut connection,
                &page(
                    Some(1),
                    UID_VALIDITY,
                    vec![envelope(2, "Wrong cursor", InboxFlags::new(false, false))],
                ),
            )
            .unwrap(),
            InboxStageOutcome::Stale
        );
        assert_eq!(
            stage_inbox_page(
                &mut connection,
                &page(
                    None,
                    UID_VALIDITY + 1,
                    vec![envelope(2, "Wrong validity", InboxFlags::new(false, false))],
                ),
            )
            .unwrap(),
            InboxStageOutcome::Stale
        );
        let after: (i64, String) = connection
            .query_row("SELECT count(*), subject FROM messages", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(after, before);

        connection
            .execute(
                "INSERT INTO message_tombstones (account_id, remote_key, deleted_at_ms)
                 VALUES (1, 'deleted', 3000)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message_tombstone_imap_locations
                 (account_id, target_key, folder_key, uid_validity, uid)
                 VALUES (1, 'deleted', 'inbox', ?1, 3)",
                [i64::from(UID_VALIDITY)],
            )
            .unwrap();
        let tombstoned = stage_inbox_page(
            &mut connection,
            &page(
                None,
                UID_VALIDITY,
                vec![envelope(3, "Deleted", InboxFlags::new(false, false))],
            ),
        )
        .unwrap();
        assert!(matches!(
            tombstoned,
            InboxStageOutcome::Staged {
                ref messages,
                tombstoned: 1,
                ref ticket,
            } if messages.is_empty() && ticket.scanned_through_uid() == Some(3)
        ));
        let resurrected: bool = connection
            .query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM imap_message_locations
                     WHERE uid_validity = ?1 AND uid = 3
                 )",
                [i64::from(UID_VALIDITY)],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!resurrected);
    }

    #[test]
    fn cursor_stale_fences_make_zero_writes() {
        let mut connection = connection();
        let (staged, generation_ticket) = stage_parts(
            stage_inbox_page(
                &mut connection,
                &page(
                    None,
                    UID_VALIDITY,
                    vec![envelope(9, "Nine", InboxFlags::new(false, false))],
                ),
            )
            .unwrap(),
        );
        add_content(&connection, &staged);

        connection
            .execute(
                "UPDATE accounts SET configuration_generation = ?1 WHERE id = 1",
                [GENERATION + 1],
            )
            .unwrap();
        let before = sync_state(&connection);
        let generation_commit = InboxCursorCommit::new(generation_ticket, 4_000).unwrap();
        assert_eq!(
            commit_inbox_cursor(&mut connection, &generation_commit).unwrap(),
            InboxCursorOutcome::Stale
        );
        assert_eq!(sync_state(&connection), before);
        connection
            .execute(
                "UPDATE accounts SET configuration_generation = ?1 WHERE id = 1",
                [GENERATION],
            )
            .unwrap();

        let (_, cursor_ticket) = stage_parts(
            stage_inbox_page(
                &mut connection,
                &page(
                    None,
                    UID_VALIDITY,
                    vec![envelope(9, "Nine", InboxFlags::new(false, false))],
                ),
            )
            .unwrap(),
        );
        connection
            .execute("UPDATE sync_state SET change_cursor = '1'", [])
            .unwrap();
        let before = sync_state(&connection);
        let cursor_commit = InboxCursorCommit::new(cursor_ticket, 4_000).unwrap();
        assert_eq!(
            commit_inbox_cursor(&mut connection, &cursor_commit).unwrap(),
            InboxCursorOutcome::Stale
        );
        assert_eq!(sync_state(&connection), before);
        connection
            .execute("UPDATE sync_state SET change_cursor = NULL", [])
            .unwrap();

        let (_, validity_ticket) = stage_parts(
            stage_inbox_page(
                &mut connection,
                &page(
                    None,
                    UID_VALIDITY,
                    vec![envelope(9, "Nine", InboxFlags::new(false, false))],
                ),
            )
            .unwrap(),
        );
        connection
            .execute(
                "UPDATE sync_state SET uid_validity = ?1",
                [i64::from(UID_VALIDITY + 1)],
            )
            .unwrap();
        let before = sync_state(&connection);
        let validity_commit = InboxCursorCommit::new(validity_ticket, 4_000).unwrap();
        assert_eq!(
            commit_inbox_cursor(&mut connection, &validity_commit).unwrap(),
            InboxCursorOutcome::Stale
        );
        assert_eq!(sync_state(&connection), before);
    }
}
