use crate::core::{
    AccountDirectoryQuery, AccountScope, FolderScope, Generation, MailboxQuery, MessageId,
    MessageQuery, PageBoundary, PageSpec, RequestId,
};
use crate::store::sqlite::{MailboxPage, MessageDetail, PageCursor, Tagged};
use crate::ui_identity::{AccountKey, EntityKey};
use std::{error::Error, fmt};

const PAGE_SIZE: u8 = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RequestStamp {
    request_id: RequestId,
    generation: Generation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DetailStamp {
    request: RequestStamp,
    message_id: MessageId,
}

pub(crate) struct ReadSession {
    next_request_id: Option<u64>,
    generation_value: u64,
    account: AccountKey,
    folder: FolderScope,
    search: Box<str>,
    visible_ids: Box<[MessageId]>,
    selected: Option<MessageId>,
    next_cursor: Option<PageCursor>,
    accounts_request: Option<RequestStamp>,
    mailbox_request: Option<RequestStamp>,
    detail_request: Option<DetailStamp>,
}

impl ReadSession {
    pub(crate) fn new() -> Self {
        Self {
            next_request_id: Some(1),
            generation_value: 1,
            account: AccountKey::All,
            folder: FolderScope::Inbox,
            search: Box::default(),
            visible_ids: Box::new([]),
            selected: None,
            next_cursor: None,
            accounts_request: None,
            mailbox_request: None,
            detail_request: None,
        }
    }

    pub(crate) fn account(&self) -> AccountKey {
        self.account
    }

    #[cfg(test)]
    fn folder(&self) -> FolderScope {
        self.folder
    }

    pub(crate) fn folder_label(&self) -> &'static str {
        folder_label(self.folder)
    }

    pub(crate) fn search(&self) -> &str {
        &self.search
    }

    pub(crate) fn accounts_pending(&self) -> bool {
        self.accounts_request.is_some()
    }

    #[cfg(test)]
    fn selected(&self) -> Option<MessageId> {
        self.selected
    }

    pub(crate) fn next_cursor(&self) -> Option<PageCursor> {
        self.next_cursor
    }

    pub(crate) fn issue_accounts(&mut self) -> Result<AccountDirectoryQuery, SessionError> {
        let request = self.next_stamp()?;
        self.accounts_request = Some(request);
        Ok(AccountDirectoryQuery::new(
            request.request_id,
            request.generation,
        ))
    }

    pub(crate) fn issue_first_mailbox(&mut self) -> Result<MailboxQuery, SessionError> {
        let spec = self.page_spec(None)?;
        let request = self.next_stamp()?;
        self.visible_ids = Box::new([]);
        self.selected = None;
        self.next_cursor = None;
        self.mailbox_request = Some(request);
        self.detail_request = None;
        Ok(MailboxQuery::new(
            request.request_id,
            request.generation,
            spec,
        ))
    }

    pub(crate) fn select_message(&mut self, key: EntityKey) -> Result<MessageQuery, SessionError> {
        let message_id = MessageId::new(key.get()).map_err(|_| SessionError::InvalidIdentity)?;
        if !self.visible_ids.contains(&message_id) {
            return Err(SessionError::MessageNotVisible);
        }
        let request = self.next_stamp()?;
        self.selected = Some(message_id);
        self.detail_request = Some(DetailStamp {
            request,
            message_id,
        });
        Ok(MessageQuery::new(
            request.request_id,
            request.generation,
            message_id,
        ))
    }

    pub(crate) fn set_account(&mut self, account: AccountKey) -> Result<bool, SessionError> {
        account_scope(account)?;
        if self.account == account {
            return Ok(false);
        }
        self.advance_generation()?;
        self.account = account;
        self.invalidate_mailbox();
        Ok(true)
    }

    pub(crate) fn set_folder(&mut self, label: &str) -> Result<bool, SessionError> {
        let folder = parse_folder(label).ok_or(SessionError::InvalidFolder)?;
        if self.folder == folder {
            return Ok(false);
        }
        self.advance_generation()?;
        self.folder = folder;
        self.invalidate_mailbox();
        Ok(true)
    }

    pub(crate) fn set_search(&mut self, value: &str) -> Result<bool, SessionError> {
        let search = value.trim();
        PageSpec::new(
            account_scope(self.account)?,
            self.folder,
            (!search.is_empty()).then_some(search),
            PageBoundary::First,
            PAGE_SIZE,
        )
        .map_err(|_| SessionError::InvalidSearch)?;
        if self.search.as_ref() == search {
            return Ok(false);
        }
        let next_search = Box::<str>::from(search);
        self.advance_generation()?;
        self.search = next_search;
        self.invalidate_mailbox();
        Ok(true)
    }

    pub(crate) fn accept_accounts(
        &mut self,
        request_id: RequestId,
        generation: Generation,
    ) -> bool {
        if self.accounts_request
            != Some(RequestStamp {
                request_id,
                generation,
            })
        {
            return false;
        }
        self.accounts_request = None;
        true
    }

    pub(crate) fn accept_mailbox(&mut self, reply: &Tagged<MailboxPage>) -> bool {
        let request = RequestStamp {
            request_id: reply.request_id,
            generation: reply.generation,
        };
        if self.mailbox_request != Some(request) {
            return false;
        }
        self.mailbox_request = None;
        if let Ok(page) = &reply.result {
            self.visible_ids = page.rows.iter().map(|row| row.id).collect();
            self.next_cursor = page.next_cursor;
        }
        true
    }

    pub(crate) fn reject_mailbox(&mut self, request_id: RequestId, generation: Generation) -> bool {
        let request = RequestStamp {
            request_id,
            generation,
        };
        if self.mailbox_request != Some(request) {
            return false;
        }
        self.mailbox_request = None;
        true
    }

    pub(crate) fn accept_message(
        &mut self,
        reply: &Tagged<Option<MessageDetail>>,
    ) -> DetailAcceptance {
        let Some(pending) = self.detail_request else {
            return DetailAcceptance::Stale;
        };
        let request = RequestStamp {
            request_id: reply.request_id,
            generation: reply.generation,
        };
        if pending.request != request
            || self.selected != Some(pending.message_id)
            || !self.visible_ids.contains(&pending.message_id)
        {
            return DetailAcceptance::Stale;
        }

        match &reply.result {
            Ok(Some(detail)) if detail.id == pending.message_id => {
                self.detail_request = None;
                DetailAcceptance::Ready
            }
            Ok(Some(_)) => {
                self.detail_request = None;
                DetailAcceptance::Failed
            }
            Ok(None) => {
                self.detail_request = None;
                self.selected = None;
                DetailAcceptance::NotFound
            }
            Err(_) => {
                self.detail_request = None;
                DetailAcceptance::Failed
            }
        }
    }

    pub(crate) fn reject_message(&mut self, request_id: RequestId, generation: Generation) -> bool {
        let Some(pending) = self.detail_request else {
            return false;
        };
        if pending.request
            != (RequestStamp {
                request_id,
                generation,
            })
        {
            return false;
        }
        self.detail_request = None;
        true
    }

    pub(crate) fn cancel_accounts_submission(&mut self) {
        self.accounts_request = None;
    }

    pub(crate) fn cancel_mailbox_submission(&mut self) {
        self.mailbox_request = None;
    }

    pub(crate) fn cancel_detail_submission(&mut self) {
        self.detail_request = None;
    }

    pub(crate) fn cancel_pending(&mut self) {
        self.accounts_request = None;
        self.mailbox_request = None;
        self.detail_request = None;
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selected = None;
        self.detail_request = None;
    }

    fn page_spec(&self, after: Option<PageCursor>) -> Result<PageSpec, SessionError> {
        PageSpec::new(
            account_scope(self.account)?,
            self.folder,
            (!self.search.is_empty()).then_some(self.search.as_ref()),
            after.map_or(PageBoundary::First, PageBoundary::After),
            PAGE_SIZE,
        )
        .map_err(|_| SessionError::InvalidSearch)
    }

    fn next_stamp(&mut self) -> Result<RequestStamp, SessionError> {
        let value = self
            .next_request_id
            .ok_or(SessionError::RequestIdExhausted)?;
        let request_id = RequestId::new(value).map_err(|_| SessionError::RequestIdExhausted)?;
        self.next_request_id = value.checked_add(1);
        Ok(RequestStamp {
            request_id,
            generation: Generation::new(self.generation_value),
        })
    }

    fn advance_generation(&mut self) -> Result<(), SessionError> {
        self.generation_value = self
            .generation_value
            .checked_add(1)
            .ok_or(SessionError::GenerationExhausted)?;
        Ok(())
    }

    fn invalidate_mailbox(&mut self) {
        self.visible_ids = Box::new([]);
        self.selected = None;
        self.next_cursor = None;
        self.mailbox_request = None;
        self.detail_request = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetailAcceptance {
    Stale,
    Ready,
    NotFound,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionError {
    InvalidIdentity,
    InvalidFolder,
    InvalidSearch,
    MessageNotVisible,
    RequestIdExhausted,
    GenerationExhausted,
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity => formatter.write_str("the selected identity is invalid"),
            Self::InvalidFolder => formatter.write_str("the selected folder is invalid"),
            Self::InvalidSearch => formatter.write_str("the search query exceeds its limit"),
            Self::MessageNotVisible => {
                formatter.write_str("the selected message is not in the current page")
            }
            Self::RequestIdExhausted => formatter.write_str("request identities are exhausted"),
            Self::GenerationExhausted => formatter.write_str("view generations are exhausted"),
        }
    }
}

impl Error for SessionError {}

fn account_scope(account: AccountKey) -> Result<AccountScope, SessionError> {
    match account {
        AccountKey::All => Ok(AccountScope::All),
        AccountKey::Account(id) => {
            AccountScope::account(id.get()).map_err(|_| SessionError::InvalidIdentity)
        }
    }
}

fn parse_folder(label: &str) -> Option<FolderScope> {
    match label {
        "Inbox" => Some(FolderScope::Inbox),
        "Starred" => Some(FolderScope::Starred),
        "Unread" => Some(FolderScope::Unread),
        "Sent" => Some(FolderScope::Sent),
        "Drafts" => Some(FolderScope::Drafts),
        "Archive" => Some(FolderScope::Archive),
        "Trash" => Some(FolderScope::Trash),
        _ => None,
    }
}

fn folder_label(folder: FolderScope) -> &'static str {
    match folder {
        FolderScope::Inbox => "Inbox",
        FolderScope::Starred => "Starred",
        FolderScope::Unread => "Unread",
        FolderScope::Sent => "Sent",
        FolderScope::Drafts => "Drafts",
        FolderScope::Archive => "Archive",
        FolderScope::Trash => "Trash",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sqlite::{AccountUnreadDto, MailSummaryDto, MailboxStatsDto, MessageId};

    fn request_id(value: u64) -> RequestId {
        RequestId::new(value).unwrap()
    }

    fn mailbox_reply(
        request_id_value: u64,
        generation_value: u64,
        ids: &[i64],
    ) -> Tagged<MailboxPage> {
        Tagged {
            request_id: request_id(request_id_value),
            generation: Generation::new(generation_value),
            result: Ok(MailboxPage {
                rows: ids
                    .iter()
                    .map(|id| MailSummaryDto {
                        id: MessageId::new(*id).unwrap(),
                        account_id: 1,
                        sender_name: "Sender".into(),
                        sender_address: "sender@example.test".into(),
                        subject: "Subject".into(),
                        preview: "Preview".into(),
                        received_at_ms: 0,
                        unread: true,
                        starred: false,
                        has_attachment: false,
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                previous_cursor: None,
                next_cursor: None,
                stats: MailboxStatsDto {
                    selected_total: Some(u64::try_from(ids.len()).unwrap()),
                    inbox_unread: 0,
                    starred_total: 0,
                    drafts_total: 0,
                    account_unread: Box::<[AccountUnreadDto]>::default(),
                },
            }),
        }
    }

    fn detail(message_id: i64) -> MessageDetail {
        MessageDetail {
            id: MessageId::new(message_id).unwrap(),
            account_id: 1,
            sender_name: "Sender".into(),
            sender_address: "sender@example.test".into(),
            subject: "Subject".into(),
            received_at_ms: 0,
            unread: false,
            starred: false,
            has_attachment: false,
            reader_excerpt: "Body".into(),
            body_truncated: false,
            body_byte_count: 4,
            body_file_key: None,
        }
    }

    #[test]
    fn initial_mailbox_result_establishes_only_its_bounded_visible_ids() {
        let mut session = ReadSession::new();
        session.issue_accounts().unwrap();
        session.issue_first_mailbox().unwrap();

        assert!(session.accept_mailbox(&mailbox_reply(2, 1, &[1, 2, 3])));
        assert!(session.select_message(EntityKey::new(3).unwrap()).is_ok());
        assert_eq!(
            session.select_message(EntityKey::new(4).unwrap()),
            Err(SessionError::MessageNotVisible)
        );
    }

    #[test]
    fn context_change_rejects_stale_mailbox_and_detail_results() {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        assert!(session.accept_mailbox(&mailbox_reply(1, 1, &[7])));
        session.select_message(EntityKey::new(7).unwrap()).unwrap();
        assert!(session.set_folder("Archive").unwrap());
        session.issue_first_mailbox().unwrap();

        assert!(!session.accept_mailbox(&mailbox_reply(1, 1, &[7])));
        let stale_detail = Tagged {
            request_id: request_id(2),
            generation: Generation::new(1),
            result: Ok(Some(detail(7))),
        };
        assert_eq!(
            session.accept_message(&stale_detail),
            DetailAcceptance::Stale
        );
        assert_eq!(session.folder_label(), "Archive");
        assert_eq!(session.selected(), None);
    }

    #[test]
    fn changing_selection_rejects_the_older_detail() {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        assert!(session.accept_mailbox(&mailbox_reply(1, 1, &[7, 8])));
        session.select_message(EntityKey::new(7).unwrap()).unwrap();
        session.select_message(EntityKey::new(8).unwrap()).unwrap();

        let old = Tagged {
            request_id: request_id(2),
            generation: Generation::new(1),
            result: Ok(Some(detail(7))),
        };
        let current = Tagged {
            request_id: request_id(3),
            generation: Generation::new(1),
            result: Ok(Some(detail(8))),
        };
        assert_eq!(session.accept_message(&old), DetailAcceptance::Stale);
        assert_eq!(session.accept_message(&current), DetailAcceptance::Ready);
        assert_eq!(session.selected(), MessageId::new(8).ok());
    }

    #[test]
    fn matching_detail_request_with_wrong_identity_fails_and_clears_pending() {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        assert!(session.accept_mailbox(&mailbox_reply(1, 1, &[7, 8])));
        session.select_message(EntityKey::new(7).unwrap()).unwrap();
        let mismatched = Tagged {
            request_id: request_id(2),
            generation: Generation::new(1),
            result: Ok(Some(detail(8))),
        };

        assert_eq!(
            session.accept_message(&mismatched),
            DetailAcceptance::Failed
        );
        assert_eq!(session.accept_message(&mismatched), DetailAcceptance::Stale);
    }

    #[test]
    fn invalid_context_changes_are_atomic() {
        let mut session = ReadSession::new();

        assert_eq!(
            session.set_folder("Unknown"),
            Err(SessionError::InvalidFolder)
        );
        assert_eq!(session.folder(), FolderScope::Inbox);
        assert_eq!(
            session.set_search(&"x".repeat(257)),
            Err(SessionError::InvalidSearch)
        );
        assert_eq!(session.search(), "");
    }

    #[test]
    fn account_request_pending_state_ends_only_for_the_matching_reply() {
        let mut session = ReadSession::new();
        session.issue_accounts().unwrap();
        assert!(session.accounts_pending());
        assert!(!session.accept_accounts(request_id(9), Generation::new(1)));
        assert!(session.accounts_pending());
        assert!(session.accept_accounts(request_id(1), Generation::new(1)));
        assert!(!session.accounts_pending());
    }

    #[test]
    fn counters_never_wrap() {
        let mut session = ReadSession::new();
        session.next_request_id = Some(u64::MAX);
        session.issue_accounts().unwrap();
        assert_eq!(
            session.issue_accounts(),
            Err(SessionError::RequestIdExhausted)
        );

        session.generation_value = u64::MAX;
        assert_eq!(
            session.set_folder("Archive"),
            Err(SessionError::GenerationExhausted)
        );
        assert_eq!(session.folder(), FolderScope::Inbox);
    }
}
