use crate::core::{
    AccountDirectoryQuery, AccountScope, FolderScope, Generation, MailboxQuery, MessageId,
    MessageQuery, PageBoundary, PageSpec, RequestId,
};
use crate::store::sqlite::{MailboxPage, MessageDetail, PageCursor, Tagged};
use crate::ui_identity::{AccountKey, EntityKey};
use std::{error::Error, fmt};

const PAGE_SIZE: u8 = 50;
const MAX_PAGE_NUMBER: u64 = i32::MAX as u64;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingMailbox {
    request: RequestStamp,
    intent: MailboxIntent,
    target_page: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MailboxIntent {
    First,
    Next,
    Previous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MailboxAcceptance {
    request: RequestStamp,
    intent: MailboxIntent,
    target_page: u64,
}

impl MailboxAcceptance {
    pub(crate) fn intent(self) -> MailboxIntent {
        self.intent
    }

    #[cfg(test)]
    pub(crate) fn target_page(self) -> u64 {
        self.target_page
    }

    pub(crate) fn stage(self, page: &MailboxPage) -> Result<MailboxCommit, SessionError> {
        if page.rows.len() > usize::from(PAGE_SIZE) {
            return Err(SessionError::MailboxPageTooLarge);
        }
        if self.intent != MailboxIntent::First && page.rows.is_empty() {
            return Err(SessionError::EmptyNavigationPage);
        }

        Ok(MailboxCommit {
            request: self.request,
            page_number: self.target_page,
            visible_ids: page.rows.iter().map(|row| row.id).collect(),
            previous_cursor: page.previous_cursor,
            next_cursor: page.next_cursor,
        })
    }
}

pub(crate) struct MailboxCommit {
    request: RequestStamp,
    page_number: u64,
    visible_ids: Box<[MessageId]>,
    previous_cursor: Option<PageCursor>,
    next_cursor: Option<PageCursor>,
}

pub(crate) struct ReadSession {
    next_request_id: Option<u64>,
    generation_value: u64,
    account: AccountKey,
    folder: FolderScope,
    search: Box<str>,
    visible_ids: Box<[MessageId]>,
    selected: Option<MessageId>,
    previous_cursor: Option<PageCursor>,
    next_cursor: Option<PageCursor>,
    page_number: u64,
    accounts_request: Option<RequestStamp>,
    mailbox_request: Option<PendingMailbox>,
    latest_mailbox_request: Option<RequestStamp>,
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
            previous_cursor: None,
            next_cursor: None,
            page_number: 0,
            accounts_request: None,
            mailbox_request: None,
            latest_mailbox_request: None,
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

    pub(crate) fn previous_cursor(&self) -> Option<PageCursor> {
        self.previous_cursor
    }

    pub(crate) fn page_number(&self) -> u64 {
        self.page_number
    }

    pub(crate) fn mailbox_pending(&self) -> bool {
        self.mailbox_request.is_some()
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
        self.issue_mailbox(MailboxIntent::First, PageBoundary::First, 1)
    }

    pub(crate) fn issue_next_mailbox(&mut self) -> Result<MailboxQuery, SessionError> {
        let cursor = self
            .next_cursor
            .ok_or(SessionError::NavigationUnavailable)?;
        let target_page = self
            .page_number
            .checked_add(1)
            .filter(|page| *page <= MAX_PAGE_NUMBER)
            .ok_or(SessionError::PageNumberExhausted)?;
        self.issue_mailbox(
            MailboxIntent::Next,
            PageBoundary::After(cursor),
            target_page,
        )
    }

    pub(crate) fn issue_previous_mailbox(&mut self) -> Result<MailboxQuery, SessionError> {
        let cursor = self
            .previous_cursor
            .ok_or(SessionError::NavigationUnavailable)?;
        let target_page = self
            .page_number
            .checked_sub(1)
            .filter(|page| *page > 0)
            .ok_or(SessionError::PageNumberExhausted)?;
        self.issue_mailbox(
            MailboxIntent::Previous,
            PageBoundary::Before(cursor),
            target_page,
        )
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

    pub(crate) fn match_mailbox_reply(
        &mut self,
        reply: &Tagged<MailboxPage>,
    ) -> Option<MailboxAcceptance> {
        let request = RequestStamp {
            request_id: reply.request_id,
            generation: reply.generation,
        };
        let pending = self
            .mailbox_request
            .filter(|pending| pending.request == request)?;
        self.mailbox_request = None;
        Some(MailboxAcceptance {
            request,
            intent: pending.intent,
            target_page: pending.target_page,
        })
    }

    pub(crate) fn commit_mailbox(&mut self, commit: MailboxCommit) -> bool {
        if self.latest_mailbox_request != Some(commit.request)
            || commit.request.generation != Generation::new(self.generation_value)
            || self.mailbox_request.is_some()
        {
            return false;
        }

        self.latest_mailbox_request = None;
        self.visible_ids = commit.visible_ids;
        self.previous_cursor = commit.previous_cursor;
        self.next_cursor = commit.next_cursor;
        self.page_number = commit.page_number;
        self.selected = None;
        self.detail_request = None;
        true
    }

    pub(crate) fn reject_mailbox(
        &mut self,
        request_id: RequestId,
        generation: Generation,
    ) -> Option<MailboxIntent> {
        let request = RequestStamp {
            request_id,
            generation,
        };
        let pending = self
            .mailbox_request
            .filter(|pending| pending.request == request)?;
        self.mailbox_request = None;
        self.latest_mailbox_request = None;
        Some(pending.intent)
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
        self.latest_mailbox_request = None;
    }

    pub(crate) fn cancel_detail_submission(&mut self) {
        self.detail_request = None;
    }

    pub(crate) fn cancel_pending(&mut self) {
        self.accounts_request = None;
        self.mailbox_request = None;
        self.latest_mailbox_request = None;
        self.detail_request = None;
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selected = None;
        self.detail_request = None;
    }

    fn page_spec(&self, boundary: PageBoundary) -> Result<PageSpec, SessionError> {
        PageSpec::new(
            account_scope(self.account)?,
            self.folder,
            (!self.search.is_empty()).then_some(self.search.as_ref()),
            boundary,
            PAGE_SIZE,
        )
        .map_err(|_| SessionError::InvalidSearch)
    }

    fn issue_mailbox(
        &mut self,
        intent: MailboxIntent,
        boundary: PageBoundary,
        target_page: u64,
    ) -> Result<MailboxQuery, SessionError> {
        if self.mailbox_request.is_some() {
            return Err(SessionError::MailboxRequestPending);
        }
        let spec = self.page_spec(boundary)?;
        let request = self.next_stamp()?;
        self.mailbox_request = Some(PendingMailbox {
            request,
            intent,
            target_page,
        });
        self.latest_mailbox_request = Some(request);
        Ok(MailboxQuery::new(
            request.request_id,
            request.generation,
            spec,
        ))
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
        self.previous_cursor = None;
        self.next_cursor = None;
        self.page_number = 0;
        self.mailbox_request = None;
        self.latest_mailbox_request = None;
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
    MailboxRequestPending,
    NavigationUnavailable,
    EmptyNavigationPage,
    MailboxPageTooLarge,
    PageNumberExhausted,
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
            Self::MailboxRequestPending => {
                formatter.write_str("a mailbox page request is already pending")
            }
            Self::NavigationUnavailable => {
                formatter.write_str("the requested mailbox page is unavailable")
            }
            Self::EmptyNavigationPage => {
                formatter.write_str("a navigation request returned an empty mailbox page")
            }
            Self::MailboxPageTooLarge => {
                formatter.write_str("the mailbox page exceeds the visible row limit")
            }
            Self::PageNumberExhausted => formatter.write_str("mailbox page numbers are exhausted"),
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
        mailbox_reply_with_cursors(request_id_value, generation_value, ids, None, None)
    }

    fn mailbox_reply_with_cursors(
        request_id_value: u64,
        generation_value: u64,
        ids: &[i64],
        previous_id: Option<i64>,
        next_id: Option<i64>,
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
                        received_at_ms: *id * 1_000,
                        unread: true,
                        starred: false,
                        has_attachment: false,
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                previous_cursor: previous_id.map(cursor),
                next_cursor: next_id.map(cursor),
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

    fn cursor(id: i64) -> PageCursor {
        PageCursor::new(id * 1_000, id).unwrap()
    }

    fn commit_reply(session: &mut ReadSession, reply: &Tagged<MailboxPage>) {
        let acceptance = session
            .match_mailbox_reply(reply)
            .expect("matching mailbox reply");
        let commit = acceptance
            .stage(reply.result.as_ref().expect("successful mailbox reply"))
            .expect("valid mailbox page");
        assert!(session.commit_mailbox(commit));
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

        commit_reply(&mut session, &mailbox_reply(2, 1, &[1, 2, 3]));
        assert!(session.select_message(EntityKey::new(3).unwrap()).is_ok());
        assert_eq!(
            session.select_message(EntityKey::new(4).unwrap()),
            Err(SessionError::MessageNotVisible)
        );
    }

    #[test]
    fn mailbox_page_changes_only_after_an_explicit_staged_commit() {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        let reply = mailbox_reply_with_cursors(1, 1, &[1, 2], None, Some(2));

        let acceptance = session.match_mailbox_reply(&reply).unwrap();
        assert_eq!(acceptance.intent(), MailboxIntent::First);
        assert_eq!(acceptance.target_page(), 1);
        assert!(!session.mailbox_pending());
        assert_eq!(session.page_number(), 0);
        assert!(session.visible_ids.is_empty());

        let commit = acceptance.stage(reply.result.as_ref().unwrap()).unwrap();
        assert_eq!(session.page_number(), 0);
        assert!(session.visible_ids.is_empty());
        assert!(session.commit_mailbox(commit));
        assert_eq!(session.page_number(), 1);
        assert_eq!(
            session.visible_ids.as_ref(),
            &[MessageId::new(1).unwrap(), MessageId::new(2).unwrap()]
        );
        assert_eq!(session.previous_cursor(), None);
        assert_eq!(session.next_cursor(), Some(cursor(2)));
    }

    #[test]
    fn next_and_previous_navigation_keep_only_the_current_page_boundaries() {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        commit_reply(
            &mut session,
            &mailbox_reply_with_cursors(1, 1, &[1, 2], None, Some(2)),
        );

        session.issue_next_mailbox().unwrap();
        let next = mailbox_reply_with_cursors(2, 1, &[3, 4], Some(3), Some(4));
        let acceptance = session.match_mailbox_reply(&next).unwrap();
        assert_eq!(acceptance.intent(), MailboxIntent::Next);
        assert_eq!(acceptance.target_page(), 2);
        assert_eq!(session.page_number(), 1);
        assert!(session.commit_mailbox(acceptance.stage(next.result.as_ref().unwrap()).unwrap()));
        assert_eq!(session.page_number(), 2);
        assert_eq!(session.previous_cursor(), Some(cursor(3)));
        assert_eq!(session.next_cursor(), Some(cursor(4)));

        session.issue_previous_mailbox().unwrap();
        let previous = mailbox_reply_with_cursors(3, 1, &[1, 2], None, Some(2));
        let acceptance = session.match_mailbox_reply(&previous).unwrap();
        assert_eq!(acceptance.intent(), MailboxIntent::Previous);
        assert_eq!(acceptance.target_page(), 1);
        assert!(
            session.commit_mailbox(acceptance.stage(previous.result.as_ref().unwrap()).unwrap())
        );
        assert_eq!(session.page_number(), 1);
        assert_eq!(session.previous_cursor(), None);
        assert_eq!(session.next_cursor(), Some(cursor(2)));
    }

    #[test]
    fn pending_mailbox_request_rejects_repeated_navigation() {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        assert_eq!(
            session.issue_first_mailbox(),
            Err(SessionError::MailboxRequestPending)
        );
        commit_reply(
            &mut session,
            &mailbox_reply_with_cursors(1, 1, &[1], None, Some(1)),
        );

        session.issue_next_mailbox().unwrap();
        assert_eq!(
            session.issue_next_mailbox(),
            Err(SessionError::MailboxRequestPending)
        );
        assert_eq!(session.page_number(), 1);
        assert_eq!(session.visible_ids.as_ref(), &[MessageId::new(1).unwrap()]);
    }

    #[test]
    fn navigation_failures_preserve_the_committed_page_and_selection() {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        commit_reply(
            &mut session,
            &mailbox_reply_with_cursors(1, 1, &[1], None, Some(1)),
        );
        session.select_message(EntityKey::new(1).unwrap()).unwrap();

        session.issue_next_mailbox().unwrap();
        session.cancel_mailbox_submission();
        assert_eq!(session.page_number(), 1);
        assert_eq!(session.selected(), MessageId::new(1).ok());

        session.issue_next_mailbox().unwrap();
        assert_eq!(
            session.reject_mailbox(request_id(4), Generation::new(1)),
            Some(MailboxIntent::Next)
        );
        assert_eq!(session.page_number(), 1);
        assert_eq!(session.selected(), MessageId::new(1).ok());

        session.issue_next_mailbox().unwrap();
        let empty = mailbox_reply(5, 1, &[]);
        let acceptance = session.match_mailbox_reply(&empty).unwrap();
        assert_eq!(
            acceptance.stage(empty.result.as_ref().unwrap()).err(),
            Some(SessionError::EmptyNavigationPage)
        );
        assert_eq!(session.page_number(), 1);
        assert_eq!(session.selected(), MessageId::new(1).ok());

        session.issue_next_mailbox().unwrap();
        let valid = mailbox_reply_with_cursors(6, 1, &[2], Some(2), None);
        let acceptance = session.match_mailbox_reply(&valid).unwrap();
        let _projected_but_discarded = acceptance.stage(valid.result.as_ref().unwrap()).unwrap();
        assert_eq!(session.page_number(), 1);
        assert_eq!(session.selected(), MessageId::new(1).ok());
        assert!(session.issue_next_mailbox().is_ok());
    }

    #[test]
    fn first_page_may_be_empty_but_visible_rows_are_always_bounded() {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(1, 1, &[]));
        assert_eq!(session.page_number(), 1);
        assert!(session.visible_ids.is_empty());

        session.issue_first_mailbox().unwrap();
        let ids = (1..=i64::from(PAGE_SIZE) + 1).collect::<Vec<_>>();
        let oversized = mailbox_reply(2, 1, &ids);
        let acceptance = session.match_mailbox_reply(&oversized).unwrap();
        assert_eq!(
            acceptance.stage(oversized.result.as_ref().unwrap()).err(),
            Some(SessionError::MailboxPageTooLarge)
        );
        assert!(session.visible_ids.is_empty());

        session.issue_first_mailbox().unwrap();
        let bounded = mailbox_reply(3, 1, &ids[..usize::from(PAGE_SIZE)]);
        commit_reply(&mut session, &bounded);
        assert_eq!(session.visible_ids.len(), usize::from(PAGE_SIZE));
    }

    #[test]
    fn a_staged_page_cannot_commit_after_context_or_request_changes() {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        let reply = mailbox_reply(1, 1, &[1]);
        let acceptance = session.match_mailbox_reply(&reply).unwrap();
        let commit = acceptance.stage(reply.result.as_ref().unwrap()).unwrap();
        assert!(session.set_folder("Archive").unwrap());
        assert!(!session.commit_mailbox(commit));
        assert_eq!(session.page_number(), 0);

        session.issue_first_mailbox().unwrap();
        assert!(
            session
                .match_mailbox_reply(&mailbox_reply(1, 1, &[1]))
                .is_none()
        );
        assert!(session.visible_ids.is_empty());
    }

    #[test]
    fn context_change_rejects_stale_mailbox_and_detail_results() {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(1, 1, &[7]));
        session.select_message(EntityKey::new(7).unwrap()).unwrap();
        assert!(session.set_folder("Archive").unwrap());
        session.issue_first_mailbox().unwrap();

        assert!(
            session
                .match_mailbox_reply(&mailbox_reply(1, 1, &[7]))
                .is_none()
        );
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
        commit_reply(&mut session, &mailbox_reply(1, 1, &[7, 8]));
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
        commit_reply(&mut session, &mailbox_reply(1, 1, &[7, 8]));
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

        let mut session = ReadSession::new();
        session.page_number = MAX_PAGE_NUMBER;
        session.next_cursor = Some(cursor(1));
        assert_eq!(
            session.issue_next_mailbox(),
            Err(SessionError::PageNumberExhausted)
        );
        assert!(!session.mailbox_pending());

        session.page_number = 0;
        session.next_cursor = None;
        session.previous_cursor = Some(cursor(1));
        assert_eq!(
            session.issue_previous_mailbox(),
            Err(SessionError::PageNumberExhausted)
        );
        assert!(!session.mailbox_pending());
    }

    #[test]
    fn navigation_requires_a_committed_boundary_cursor() {
        let mut session = ReadSession::new();
        assert_eq!(
            session.issue_next_mailbox(),
            Err(SessionError::NavigationUnavailable)
        );
        assert_eq!(
            session.issue_previous_mailbox(),
            Err(SessionError::NavigationUnavailable)
        );
    }
}
