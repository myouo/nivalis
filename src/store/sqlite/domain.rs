use std::{
    fmt,
    num::{NonZeroI64, NonZeroU64},
};

pub(super) const MAX_PAGE_SIZE: u8 = 50;
pub(super) const MAX_SEARCH_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RequestId(NonZeroU64);

impl RequestId {
    pub(crate) fn new(value: u64) -> Result<Self, ValidationError> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(ValidationError::ZeroRequestId)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Generation(u64);

impl Generation {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MessageId(i64);

impl MessageId {
    pub(crate) fn new(value: i64) -> Result<Self, ValidationError> {
        if value > 0 {
            Ok(Self(value))
        } else {
            Err(ValidationError::InvalidMessageId(value))
        }
    }

    pub(super) fn from_database(value: i64) -> Self {
        debug_assert!(value > 0);
        Self(value)
    }

    pub(crate) fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AccountId(NonZeroI64);

impl AccountId {
    fn new(value: i64) -> Result<Self, ValidationError> {
        if value > 0 {
            Ok(Self(
                NonZeroI64::new(value).expect("positive account id is non-zero"),
            ))
        } else {
            Err(ValidationError::InvalidAccountId(value))
        }
    }

    fn get(self) -> i64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountScope {
    All,
    Account(AccountId),
}

impl AccountScope {
    pub(crate) fn account(value: i64) -> Result<Self, ValidationError> {
        AccountId::new(value).map(Self::Account)
    }

    pub(super) fn database_id(self) -> Option<i64> {
        match self {
            Self::All => None,
            Self::Account(id) => Some(id.get()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FolderScope {
    Inbox,
    Starred,
    Unread,
    Sent,
    Drafts,
    Archive,
    Trash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PageCursor {
    pub(crate) received_at_ms: i64,
    pub(crate) message_id: MessageId,
}

impl PageCursor {
    pub(crate) fn new(received_at_ms: i64, message_id: i64) -> Result<Self, ValidationError> {
        Ok(Self {
            received_at_ms,
            message_id: MessageId::new(message_id)?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PageSpec {
    pub(super) account: AccountScope,
    pub(super) folder: FolderScope,
    pub(super) search: Option<Box<str>>,
    pub(super) after: Option<PageCursor>,
    pub(super) limit: u8,
}

impl PageSpec {
    pub(crate) fn new(
        account: AccountScope,
        folder: FolderScope,
        search: Option<&str>,
        after: Option<PageCursor>,
        limit: u8,
    ) -> Result<Self, ValidationError> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(ValidationError::InvalidPageSize(limit));
        }

        let search = search
            .map(str::trim)
            .filter(|search| !search.is_empty())
            .map(|search| {
                if search.len() > MAX_SEARCH_BYTES {
                    Err(ValidationError::SearchTooLong {
                        bytes: search.len(),
                    })
                } else {
                    Ok(Box::<str>::from(search))
                }
            })
            .transpose()?;

        Ok(Self {
            account,
            folder,
            search,
            after,
            limit,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MailSummaryDto {
    pub(crate) id: MessageId,
    pub(crate) account_id: i64,
    pub(crate) sender_name: Box<str>,
    pub(crate) sender_address: Box<str>,
    pub(crate) subject: Box<str>,
    pub(crate) preview: Box<str>,
    pub(crate) received_at_ms: i64,
    pub(crate) unread: bool,
    pub(crate) starred: bool,
    pub(crate) has_attachment: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MailboxPage {
    pub(crate) rows: Box<[MailSummaryDto]>,
    pub(crate) next_cursor: Option<PageCursor>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MessageDetail {
    pub(crate) id: MessageId,
    pub(crate) sender_name: Box<str>,
    pub(crate) sender_address: Box<str>,
    pub(crate) subject: Box<str>,
    pub(crate) received_at_ms: i64,
    pub(crate) reader_excerpt: Box<str>,
    pub(crate) body_truncated: bool,
    pub(crate) body_byte_count: u64,
    pub(crate) body_file_key: Option<Box<str>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Tagged<T> {
    pub(crate) request_id: RequestId,
    pub(crate) generation: Generation,
    pub(crate) result: Result<T, DbFailure>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DbReply {
    Mailbox(Tagged<MailboxPage>),
    Message(Tagged<Option<MessageDetail>>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureKind {
    Database,
    Migration,
    ResourceLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DbFailure {
    pub(crate) kind: FailureKind,
    pub(crate) message: Box<str>,
}

impl DbFailure {
    pub(super) fn database(error: impl fmt::Display) -> Self {
        Self {
            kind: FailureKind::Database,
            message: error.to_string().into_boxed_str(),
        }
    }

    pub(super) fn migration(error: impl fmt::Display) -> Self {
        Self {
            kind: FailureKind::Migration,
            message: error.to_string().into_boxed_str(),
        }
    }

    pub(super) fn resource_limit(message: impl Into<Box<str>>) -> Self {
        Self {
            kind: FailureKind::ResourceLimit,
            message: message.into(),
        }
    }
}

impl fmt::Display for DbFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DbFailure {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValidationError {
    ZeroRequestId,
    InvalidAccountId(i64),
    InvalidMessageId(i64),
    InvalidPageSize(u8),
    SearchTooLong { bytes: usize },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroRequestId => formatter.write_str("request id must be non-zero"),
            Self::InvalidAccountId(id) => {
                write!(formatter, "account id must be positive, got {id}")
            }
            Self::InvalidMessageId(id) => {
                write!(formatter, "message id must be positive, got {id}")
            }
            Self::InvalidPageSize(limit) => write!(
                formatter,
                "mailbox page size must be between 1 and {MAX_PAGE_SIZE}, got {limit}"
            ),
            Self::SearchTooLong { bytes } => write!(
                formatter,
                "search text exceeds the {MAX_SEARCH_BYTES}-byte limit ({bytes} bytes)"
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_spec_enforces_memory_bounds_before_enqueue() {
        assert!(PageSpec::new(AccountScope::All, FolderScope::Inbox, None, None, 0).is_err());
        assert!(
            PageSpec::new(
                AccountScope::All,
                FolderScope::Inbox,
                Some(&"x".repeat(MAX_SEARCH_BYTES + 1)),
                None,
                MAX_PAGE_SIZE,
            )
            .is_err()
        );
        assert!(
            PageSpec::new(
                AccountScope::All,
                FolderScope::Inbox,
                Some("  release status  "),
                None,
                MAX_PAGE_SIZE,
            )
            .is_ok()
        );
    }
}
