use crate::core::{
    AccountDirectoryQuery, AccountScope, FolderScope, Generation, MailboxQuery, MessageId,
    MessageMutation, MessageQuery, MutationOutcome, MutationRequest, PageBoundary, PageSpec,
    RequestId, UndoToken,
};
use crate::store::sqlite::{FailureKind, MailboxPage, MessageDetail, PageCursor, Tagged};
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
    mutation_refresh: Option<RequestStamp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingMutation {
    request: RequestStamp,
    intent: MutationIntent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UndoSlot {
    message_id: MessageId,
    token: UndoToken,
    expires_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MailboxIntent {
    First,
    Append,
    Refresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MailboxCommitEffect {
    Replace,
    Append,
    Extend { from: usize },
    Preserve,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MailboxAcceptance {
    request: RequestStamp,
    intent: MailboxIntent,
    target_page: u64,
    mutation_refresh: Option<RequestStamp>,
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
        if self.intent == MailboxIntent::Append && page.rows.is_empty() {
            return Err(SessionError::EmptyNavigationPage);
        }

        Ok(MailboxCommit {
            request: self.request,
            intent: self.intent,
            page_number: self.target_page,
            visible_ids: page.rows.iter().map(|row| row.id).collect(),
            next_cursor: page.next_cursor,
            mutation_refresh: self.mutation_refresh,
        })
    }
}

pub(crate) struct MailboxCommit {
    request: RequestStamp,
    intent: MailboxIntent,
    page_number: u64,
    visible_ids: Box<[MessageId]>,
    next_cursor: Option<PageCursor>,
    mutation_refresh: Option<RequestStamp>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MailAction {
    SetUnread(bool),
    SetStarred(bool),
    Archive,
    Delete,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MutationIntent {
    SetUnread { id: MessageId, unread: bool },
    SetStarred { id: MessageId, starred: bool },
    Archive { id: MessageId },
    MoveToTrash { id: MessageId },
    DeletePermanently { id: MessageId },
    UndoTrash { id: MessageId, token: UndoToken },
}

impl MutationIntent {
    fn message_id(self) -> MessageId {
        match self {
            Self::SetUnread { id, .. }
            | Self::SetStarred { id, .. }
            | Self::Archive { id }
            | Self::MoveToTrash { id }
            | Self::DeletePermanently { id }
            | Self::UndoTrash { id, .. } => id,
        }
    }

    fn mutation(self) -> MessageMutation {
        match self {
            Self::SetUnread { id, unread } => MessageMutation::set_unread(id, unread),
            Self::SetStarred { id, starred } => MessageMutation::set_starred(id, starred),
            Self::Archive { id } => MessageMutation::archive(id),
            Self::MoveToTrash { id } => MessageMutation::move_to_trash(id),
            Self::DeletePermanently { id } => MessageMutation::delete_permanently(id),
            Self::UndoTrash { token, .. } => MessageMutation::undo_trash(token),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MutationScope {
    Current,
    Changed,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MutationCompletion {
    Stale,
    Applied {
        intent: MutationIntent,
        scope: MutationScope,
    },
    Failed {
        intent: MutationIntent,
        scope: MutationScope,
    },
    OutcomeMismatch {
        intent: MutationIntent,
        scope: MutationScope,
    },
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
    page_number: u64,
    accounts_request: Option<RequestStamp>,
    mailbox_request: Option<PendingMailbox>,
    latest_mailbox_request: Option<RequestStamp>,
    detail_request: Option<DetailStamp>,
    pending_mutation: Option<PendingMutation>,
    mutation_refresh: Option<RequestStamp>,
    undo: Option<UndoSlot>,
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
            page_number: 0,
            accounts_request: None,
            mailbox_request: None,
            latest_mailbox_request: None,
            detail_request: None,
            pending_mutation: None,
            mutation_refresh: None,
            undo: None,
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

    pub(crate) fn shows_inbox_sync_updates(&self) -> bool {
        matches!(
            self.folder,
            FolderScope::Inbox | FolderScope::Starred | FolderScope::Unread
        )
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

    #[cfg(test)]
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

    pub(crate) fn issue_more_mailbox(&mut self) -> Result<MailboxQuery, SessionError> {
        let cursor = self
            .next_cursor
            .ok_or(SessionError::NavigationUnavailable)?;
        let target_page = self
            .page_number
            .checked_add(1)
            .filter(|page| *page <= MAX_PAGE_NUMBER)
            .ok_or(SessionError::PageNumberExhausted)?;
        self.issue_mailbox(
            MailboxIntent::Append,
            PageBoundary::After(cursor),
            target_page,
        )
    }

    pub(crate) fn issue_mailbox_refresh(&mut self) -> Result<MailboxQuery, SessionError> {
        self.issue_mailbox(MailboxIntent::Refresh, PageBoundary::First, 1)
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

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn issue_message_action(
        &mut self,
        key: EntityKey,
        action: MailAction,
    ) -> Result<MutationRequest, SessionError> {
        self.ensure_mutation_available()?;
        let id = MessageId::new(key.get()).map_err(|_| SessionError::InvalidIdentity)?;
        if !self.visible_ids.contains(&id) {
            return Err(SessionError::MessageNotVisible);
        }
        let intent = match action {
            MailAction::SetUnread(unread) => MutationIntent::SetUnread { id, unread },
            MailAction::SetStarred(starred) => MutationIntent::SetStarred { id, starred },
            MailAction::Archive => MutationIntent::Archive { id },
            MailAction::Delete if self.folder == FolderScope::Trash => {
                MutationIntent::DeletePermanently { id }
            }
            MailAction::Delete => MutationIntent::MoveToTrash { id },
        };
        self.issue_mutation(intent)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn issue_undo(&mut self, now_ms: i64) -> Result<MutationRequest, SessionError> {
        if self.pending_mutation.is_some() {
            return Err(SessionError::MutationRequestPending);
        }
        let Some(undo) = self.undo else {
            return Err(SessionError::UndoUnavailable);
        };
        if now_ms > undo.expires_at_ms {
            self.undo = None;
            return Err(SessionError::UndoExpired);
        }
        self.issue_mutation(MutationIntent::UndoTrash {
            id: undo.message_id,
            token: undo.token,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn complete_mutation(
        &mut self,
        reply: &Tagged<MutationOutcome>,
    ) -> MutationCompletion {
        let request = RequestStamp {
            request_id: reply.request_id,
            generation: reply.generation,
        };
        let Some(pending) = self
            .pending_mutation
            .filter(|pending| pending.request == request)
        else {
            return MutationCompletion::Stale;
        };
        self.pending_mutation = None;
        let scope = self.mutation_scope(request);

        match &reply.result {
            Err(failure) => {
                self.apply_mutation_failure(pending.intent, failure.kind);
                MutationCompletion::Failed {
                    intent: pending.intent,
                    scope,
                }
            }
            Ok(outcome) if mutation_outcome_matches(pending.intent, outcome) => {
                self.apply_mutation_outcome(pending.intent, outcome);
                self.require_mutation_refresh(request);
                MutationCompletion::Applied {
                    intent: pending.intent,
                    scope,
                }
            }
            Ok(_) => {
                self.undo = None;
                self.require_mutation_refresh(request);
                MutationCompletion::OutcomeMismatch {
                    intent: pending.intent,
                    scope,
                }
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn reject_mutation(
        &mut self,
        request_id: RequestId,
        generation: Generation,
    ) -> Option<(MutationIntent, MutationScope)> {
        let request = RequestStamp {
            request_id,
            generation,
        };
        let pending = self
            .pending_mutation
            .filter(|pending| pending.request == request)?;
        self.pending_mutation = None;
        Some((pending.intent, self.mutation_scope(request)))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn cancel_mutation_submission(&mut self) -> Option<MutationIntent> {
        self.pending_mutation.take().map(|pending| pending.intent)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn mutation_blocks_actions(&self) -> bool {
        self.pending_mutation.is_some() || self.mutation_refresh.is_some()
    }

    pub(crate) fn mutation_request_pending(&self) -> bool {
        self.pending_mutation.is_some()
    }

    pub(crate) fn mutation_refresh_pending(&self) -> bool {
        self.mutation_refresh.is_some()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn undo_expires_at_ms(&mut self, now_ms: i64) -> Option<i64> {
        let undo = self.undo?;
        if now_ms > undo.expires_at_ms {
            self.undo = None;
            None
        } else {
            Some(undo.expires_at_ms)
        }
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
            mutation_refresh: pending.mutation_refresh,
        })
    }

    pub(crate) fn commit_mailbox(&mut self, commit: MailboxCommit) -> Option<MailboxCommitEffect> {
        if self.latest_mailbox_request != Some(commit.request)
            || commit.request.generation != Generation::new(self.generation_value)
            || self.mailbox_request.is_some()
        {
            return None;
        }

        self.latest_mailbox_request = None;
        let effect = match commit.intent {
            MailboxIntent::Append => {
                if commit
                    .visible_ids
                    .iter()
                    .any(|id| self.visible_ids.contains(id))
                {
                    return None;
                }
                let mut visible_ids = self.visible_ids.to_vec();
                visible_ids.extend_from_slice(&commit.visible_ids);
                self.visible_ids = visible_ids.into_boxed_slice();
                self.next_cursor = commit.next_cursor;
                self.page_number = commit.page_number;
                MailboxCommitEffect::Append
            }
            MailboxIntent::First => {
                self.visible_ids = commit.visible_ids;
                self.next_cursor = commit.next_cursor;
                self.page_number = commit.page_number;
                self.selected = None;
                self.detail_request = None;
                MailboxCommitEffect::Replace
            }
            MailboxIntent::Refresh => {
                if commit.visible_ids.starts_with(&self.visible_ids) {
                    let from = self.visible_ids.len();
                    let extended = commit.visible_ids.len() > from;
                    self.visible_ids = commit.visible_ids;
                    self.next_cursor = commit.next_cursor;
                    self.page_number = commit.page_number;
                    if extended {
                        MailboxCommitEffect::Extend { from }
                    } else {
                        MailboxCommitEffect::Preserve
                    }
                } else if self.visible_ids.starts_with(&commit.visible_ids) {
                    MailboxCommitEffect::Preserve
                } else {
                    self.visible_ids = commit.visible_ids;
                    self.next_cursor = commit.next_cursor;
                    self.page_number = commit.page_number;
                    self.selected = None;
                    self.detail_request = None;
                    MailboxCommitEffect::Replace
                }
            }
        };
        if commit.mutation_refresh.is_some()
            && self.mutation_refresh == commit.mutation_refresh
            && commit.page_number == 1
        {
            self.mutation_refresh = None;
        }
        Some(effect)
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
        self.pending_mutation = None;
        self.mutation_refresh = None;
        self.undo = None;
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
            mutation_refresh: if intent == MailboxIntent::First {
                self.mutation_refresh
            } else {
                None
            },
        });
        self.latest_mailbox_request = Some(request);
        Ok(MailboxQuery::new(
            request.request_id,
            request.generation,
            spec,
        ))
    }

    fn ensure_mutation_available(&self) -> Result<(), SessionError> {
        if self.pending_mutation.is_some() {
            Err(SessionError::MutationRequestPending)
        } else if self.mutation_refresh.is_some() {
            Err(SessionError::MutationRefreshPending)
        } else {
            Ok(())
        }
    }

    fn issue_mutation(&mut self, intent: MutationIntent) -> Result<MutationRequest, SessionError> {
        let request = self.next_stamp()?;
        self.pending_mutation = Some(PendingMutation { request, intent });
        Ok(MutationRequest::new(
            request.request_id,
            request.generation,
            intent.mutation(),
        ))
    }

    fn mutation_scope(&self, request: RequestStamp) -> MutationScope {
        if request.generation == Generation::new(self.generation_value) {
            MutationScope::Current
        } else {
            MutationScope::Changed
        }
    }

    fn apply_mutation_failure(&mut self, intent: MutationIntent, kind: FailureKind) {
        let clears_undo = kind == FailureKind::NotFound
            || matches!(intent, MutationIntent::UndoTrash { .. }) && kind == FailureKind::Conflict;
        if clears_undo {
            self.clear_undo_for(intent.message_id());
        }
    }

    fn apply_mutation_outcome(&mut self, intent: MutationIntent, outcome: &MutationOutcome) {
        match (intent, outcome) {
            (MutationIntent::MoveToTrash { id }, MutationOutcome::MovedToTrash { undo, .. }) => {
                self.undo = Some(UndoSlot {
                    message_id: id,
                    token: undo.token,
                    expires_at_ms: undo.expires_at_ms,
                });
            }
            (MutationIntent::UndoTrash { id, .. }, MutationOutcome::Restored { .. })
            | (
                MutationIntent::DeletePermanently { id },
                MutationOutcome::PermanentlyDeleted { .. },
            ) => self.clear_undo_for(id),
            _ => {}
        }
    }

    fn clear_undo_for(&mut self, message_id: MessageId) {
        if self.undo.is_some_and(|undo| undo.message_id == message_id) {
            self.undo = None;
        }
    }

    fn require_mutation_refresh(&mut self, request: RequestStamp) {
        self.accounts_request = None;
        self.invalidate_mailbox();
        self.mutation_refresh = Some(request);
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
        self.page_number = 0;
        self.mailbox_request = None;
        self.latest_mailbox_request = None;
        self.detail_request = None;
    }
}

fn mutation_outcome_matches(intent: MutationIntent, outcome: &MutationOutcome) -> bool {
    match (intent, outcome) {
        (MutationIntent::SetUnread { id, unread }, MutationOutcome::Updated { state, .. }) => {
            state.id == id && state.unread == unread
        }
        (MutationIntent::SetStarred { id, starred }, MutationOutcome::Updated { state, .. }) => {
            state.id == id && state.starred == starred
        }
        (MutationIntent::Archive { id }, MutationOutcome::Archived { state, .. })
        | (MutationIntent::MoveToTrash { id }, MutationOutcome::MovedToTrash { state, .. })
        | (MutationIntent::UndoTrash { id, .. }, MutationOutcome::Restored { state, .. }) => {
            state.id == id
        }
        (
            MutationIntent::DeletePermanently { id },
            MutationOutcome::PermanentlyDeleted { id: deleted, .. },
        ) => id == *deleted,
        _ => false,
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
    MutationRequestPending,
    MutationRefreshPending,
    UndoUnavailable,
    UndoExpired,
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
            Self::MutationRequestPending => {
                formatter.write_str("a message change is already pending")
            }
            Self::MutationRefreshPending => {
                formatter.write_str("message changes are waiting for a refreshed mailbox")
            }
            Self::UndoUnavailable => formatter.write_str("there is no Trash action to undo"),
            Self::UndoExpired => formatter.write_str("the Trash undo period has expired"),
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
    use crate::store::sqlite::{
        AccountStatsDelta, AccountUnreadDto, DbFailure, MailSummaryDto, MailboxStatsDto, MessageId,
        MessageState, UndoReceipt, undo_token_for_test,
    };

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
        assert!(session.commit_mailbox(commit).is_some());
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

    fn stats_delta() -> AccountStatsDelta {
        AccountStatsDelta {
            account_id: 1,
            inbox_total: 0,
            inbox_unread: 0,
            starred_total: 0,
            sent_total: 0,
            drafts_total: 0,
            archive_total: 0,
            trash_total: 0,
        }
    }

    fn message_state(id: i64, unread: bool, starred: bool) -> MessageState {
        MessageState {
            id: MessageId::new(id).unwrap(),
            account_id: 1,
            revision: 1,
            unread,
            starred,
        }
    }

    fn mutation_reply(
        request_id_value: u64,
        generation_value: u64,
        outcome: MutationOutcome,
    ) -> Tagged<MutationOutcome> {
        Tagged {
            request_id: request_id(request_id_value),
            generation: Generation::new(generation_value),
            result: Ok(outcome),
        }
    }

    fn mutation_failure(
        request_id_value: u64,
        generation_value: u64,
        kind: FailureKind,
    ) -> Tagged<MutationOutcome> {
        Tagged {
            request_id: request_id(request_id_value),
            generation: Generation::new(generation_value),
            result: Err(DbFailure {
                kind,
                message: "test failure".into(),
            }),
        }
    }

    fn updated(id: i64, unread: bool, starred: bool) -> MutationOutcome {
        MutationOutcome::Updated {
            state: message_state(id, unread, starred),
            changed: true,
            stats_delta: stats_delta(),
        }
    }

    fn moved_to_trash(id: i64, token: i64, expires_at_ms: i64) -> MutationOutcome {
        MutationOutcome::MovedToTrash {
            state: message_state(id, true, false),
            undo: UndoReceipt {
                token: undo_token_for_test(token),
                expires_at_ms,
            },
            stats_delta: stats_delta(),
        }
    }

    fn session_with_visible(ids: &[i64]) -> ReadSession {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(1, 1, ids));
        session
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
        assert!(session.commit_mailbox(commit).is_some());
        assert_eq!(session.page_number(), 1);
        assert_eq!(
            session.visible_ids.as_ref(),
            &[MessageId::new(1).unwrap(), MessageId::new(2).unwrap()]
        );
        assert_eq!(session.next_cursor(), Some(cursor(2)));
    }

    #[test]
    fn appended_mailbox_batches_extend_the_visible_window_and_preserve_selection() {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        commit_reply(
            &mut session,
            &mailbox_reply_with_cursors(1, 1, &[1, 2], None, Some(2)),
        );

        session.select_message(EntityKey::new(1).unwrap()).unwrap();
        session.issue_more_mailbox().unwrap();
        let next = mailbox_reply_with_cursors(3, 1, &[3, 4], Some(3), Some(4));
        let acceptance = session.match_mailbox_reply(&next).unwrap();
        assert_eq!(acceptance.intent(), MailboxIntent::Append);
        assert_eq!(acceptance.target_page(), 2);
        assert_eq!(session.page_number(), 1);
        assert!(
            session
                .commit_mailbox(acceptance.stage(next.result.as_ref().unwrap()).unwrap())
                .is_some()
        );
        assert_eq!(session.page_number(), 2);
        assert_eq!(session.next_cursor(), Some(cursor(4)));
        assert_eq!(
            session.visible_ids.as_ref(),
            &[
                MessageId::new(1).unwrap(),
                MessageId::new(2).unwrap(),
                MessageId::new(3).unwrap(),
                MessageId::new(4).unwrap(),
            ]
        );
        assert_eq!(session.selected(), MessageId::new(1).ok());
    }

    #[test]
    fn historical_refresh_extends_a_short_list_without_losing_selection() {
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        commit_reply(
            &mut session,
            &mailbox_reply_with_cursors(1, 1, &[1, 2], None, None),
        );
        session.select_message(EntityKey::new(1).unwrap()).unwrap();

        session.issue_mailbox_refresh().unwrap();
        let refreshed = mailbox_reply_with_cursors(3, 1, &[1, 2, 3], None, Some(3));
        let acceptance = session.match_mailbox_reply(&refreshed).unwrap();
        assert_eq!(acceptance.intent(), MailboxIntent::Refresh);
        assert_eq!(
            session.commit_mailbox(
                acceptance
                    .stage(refreshed.result.as_ref().unwrap())
                    .unwrap()
            ),
            Some(MailboxCommitEffect::Extend { from: 2 })
        );
        assert_eq!(session.selected(), MessageId::new(1).ok());
        assert_eq!(session.next_cursor(), Some(cursor(3)));
    }

    #[test]
    fn historical_refresh_preserves_an_appended_window_and_its_cursor() {
        let first = (1..=50).collect::<Vec<_>>();
        let mut session = ReadSession::new();
        session.issue_first_mailbox().unwrap();
        commit_reply(
            &mut session,
            &mailbox_reply_with_cursors(1, 1, &first, None, Some(50)),
        );
        session.issue_more_mailbox().unwrap();
        let appended = mailbox_reply_with_cursors(2, 1, &[51], Some(51), Some(51));
        let acceptance = session.match_mailbox_reply(&appended).unwrap();
        assert_eq!(
            session.commit_mailbox(acceptance.stage(appended.result.as_ref().unwrap()).unwrap()),
            Some(MailboxCommitEffect::Append)
        );

        session.issue_mailbox_refresh().unwrap();
        let refreshed = mailbox_reply_with_cursors(3, 1, &first, None, Some(50));
        let acceptance = session.match_mailbox_reply(&refreshed).unwrap();
        assert_eq!(
            session.commit_mailbox(
                acceptance
                    .stage(refreshed.result.as_ref().unwrap())
                    .unwrap()
            ),
            Some(MailboxCommitEffect::Preserve)
        );
        assert_eq!(session.visible_ids.len(), 51);
        assert_eq!(session.next_cursor(), Some(cursor(51)));
        assert_eq!(session.page_number(), 2);
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

        session.issue_more_mailbox().unwrap();
        assert_eq!(
            session.issue_more_mailbox(),
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

        session.issue_more_mailbox().unwrap();
        session.cancel_mailbox_submission();
        assert_eq!(session.page_number(), 1);
        assert_eq!(session.selected(), MessageId::new(1).ok());

        session.issue_more_mailbox().unwrap();
        assert_eq!(
            session.reject_mailbox(request_id(4), Generation::new(1)),
            Some(MailboxIntent::Append)
        );
        assert_eq!(session.page_number(), 1);
        assert_eq!(session.selected(), MessageId::new(1).ok());

        session.issue_more_mailbox().unwrap();
        let empty = mailbox_reply(5, 1, &[]);
        let acceptance = session.match_mailbox_reply(&empty).unwrap();
        assert_eq!(
            acceptance.stage(empty.result.as_ref().unwrap()).err(),
            Some(SessionError::EmptyNavigationPage)
        );
        assert_eq!(session.page_number(), 1);
        assert_eq!(session.selected(), MessageId::new(1).ok());

        session.issue_more_mailbox().unwrap();
        let valid = mailbox_reply_with_cursors(6, 1, &[2], Some(2), None);
        let acceptance = session.match_mailbox_reply(&valid).unwrap();
        let _projected_but_discarded = acceptance.stage(valid.result.as_ref().unwrap()).unwrap();
        assert_eq!(session.page_number(), 1);
        assert_eq!(session.selected(), MessageId::new(1).ok());
        assert!(session.issue_more_mailbox().is_ok());
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
        assert!(session.commit_mailbox(commit).is_none());
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
            session.issue_more_mailbox(),
            Err(SessionError::PageNumberExhausted)
        );
        assert!(!session.mailbox_pending());
    }

    #[test]
    fn mutation_requests_are_single_flight_and_exactly_fenced() {
        let mut session = session_with_visible(&[1]);
        let expected = MutationRequest::new(
            request_id(2),
            Generation::new(1),
            MessageMutation::set_starred(MessageId::new(1).unwrap(), true),
        );
        assert_eq!(
            session
                .issue_message_action(EntityKey::new(1).unwrap(), MailAction::SetStarred(true))
                .unwrap(),
            expected
        );
        assert_eq!(
            session.issue_message_action(EntityKey::new(1).unwrap(), MailAction::Archive),
            Err(SessionError::MutationRequestPending)
        );
        assert_eq!(
            session.complete_mutation(&mutation_reply(9, 1, updated(1, true, true))),
            MutationCompletion::Stale
        );
        assert_eq!(
            session.reject_mutation(request_id(9), Generation::new(1)),
            None
        );
        assert!(session.mutation_blocks_actions());
        assert_eq!(
            session.reject_mutation(request_id(2), Generation::new(1)),
            Some((
                MutationIntent::SetStarred {
                    id: MessageId::new(1).unwrap(),
                    starred: true,
                },
                MutationScope::Current,
            ))
        );
        assert!(!session.mutation_blocks_actions());

        assert_eq!(
            session.issue_message_action(EntityKey::new(2).unwrap(), MailAction::Archive),
            Err(SessionError::MessageNotVisible)
        );
        assert_eq!(
            session
                .issue_message_action(EntityKey::new(1).unwrap(), MailAction::Archive)
                .unwrap(),
            MutationRequest::new(
                request_id(3),
                Generation::new(1),
                MessageMutation::archive(MessageId::new(1).unwrap()),
            )
        );
    }

    #[test]
    fn scope_change_preserves_a_write_and_requires_a_new_authoritative_first_page() {
        let mut session = session_with_visible(&[1]);
        session
            .issue_message_action(EntityKey::new(1).unwrap(), MailAction::SetStarred(true))
            .unwrap();
        assert!(session.set_folder("Archive").unwrap());

        assert_eq!(
            session.complete_mutation(&mutation_reply(2, 1, updated(1, true, true))),
            MutationCompletion::Applied {
                intent: MutationIntent::SetStarred {
                    id: MessageId::new(1).unwrap(),
                    starred: true,
                },
                scope: MutationScope::Changed,
            }
        );
        assert!(session.mutation_blocks_actions());
        assert_eq!(
            session.issue_message_action(EntityKey::new(1).unwrap(), MailAction::SetUnread(false)),
            Err(SessionError::MutationRefreshPending)
        );

        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(3, 2, &[1]));
        assert!(!session.mutation_blocks_actions());
        assert_eq!(
            session
                .issue_message_action(EntityKey::new(1).unwrap(), MailAction::Archive)
                .unwrap(),
            MutationRequest::new(
                request_id(4),
                Generation::new(2),
                MessageMutation::archive(MessageId::new(1).unwrap()),
            )
        );
    }

    #[test]
    fn a_page_staged_before_mutation_success_cannot_clear_the_refresh_barrier() {
        let mut session = session_with_visible(&[1]);
        session.issue_first_mailbox().unwrap();
        let old_reply = mailbox_reply(2, 1, &[1]);
        let old_commit = session
            .match_mailbox_reply(&old_reply)
            .unwrap()
            .stage(old_reply.result.as_ref().unwrap())
            .unwrap();

        session
            .issue_message_action(EntityKey::new(1).unwrap(), MailAction::SetUnread(false))
            .unwrap();
        assert!(matches!(
            session.complete_mutation(&mutation_reply(3, 1, updated(1, false, false))),
            MutationCompletion::Applied { .. }
        ));
        assert!(session.commit_mailbox(old_commit).is_none());
        assert!(session.mutation_blocks_actions());

        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(4, 1, &[1]));
        assert!(!session.mutation_blocks_actions());
    }

    #[test]
    fn mutation_outcome_kind_identity_and_desired_state_must_match() {
        let mut session = session_with_visible(&[1]);
        session
            .issue_message_action(EntityKey::new(1).unwrap(), MailAction::SetStarred(true))
            .unwrap();
        assert_eq!(
            session.complete_mutation(&mutation_reply(2, 1, updated(1, true, false))),
            MutationCompletion::OutcomeMismatch {
                intent: MutationIntent::SetStarred {
                    id: MessageId::new(1).unwrap(),
                    starred: true,
                },
                scope: MutationScope::Current,
            }
        );
        assert!(session.mutation_blocks_actions());

        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(3, 1, &[1]));
        session
            .issue_message_action(EntityKey::new(1).unwrap(), MailAction::Archive)
            .unwrap();
        assert!(matches!(
            session.complete_mutation(&mutation_reply(4, 1, updated(1, true, false))),
            MutationCompletion::OutcomeMismatch { .. }
        ));
    }

    #[test]
    fn trash_undo_uses_one_absolute_deadline_slot() {
        let mut session = session_with_visible(&[1]);
        session
            .issue_message_action(EntityKey::new(1).unwrap(), MailAction::Delete)
            .unwrap();
        assert!(matches!(
            session.complete_mutation(&mutation_reply(2, 1, moved_to_trash(1, 11, 100))),
            MutationCompletion::Applied { .. }
        ));
        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(3, 1, &[1]));

        assert_eq!(session.undo_expires_at_ms(100), Some(100));
        assert_eq!(
            session.issue_undo(100).unwrap(),
            MutationRequest::new(
                request_id(4),
                Generation::new(1),
                MessageMutation::undo_trash(undo_token_for_test(11)),
            )
        );
        assert!(matches!(
            session.cancel_mutation_submission(),
            Some(MutationIntent::UndoTrash { .. })
        ));
        assert_eq!(session.issue_undo(101), Err(SessionError::UndoExpired));
        assert_eq!(session.undo_expires_at_ms(101), None);
        assert_eq!(session.issue_undo(101), Err(SessionError::UndoUnavailable));
    }

    #[test]
    fn undo_can_replace_a_pending_trash_refresh_without_accepting_its_late_page() {
        let mut session = session_with_visible(&[1]);
        session
            .issue_message_action(EntityKey::new(1).unwrap(), MailAction::Delete)
            .unwrap();
        session.complete_mutation(&mutation_reply(2, 1, moved_to_trash(1, 11, 100)));

        session.issue_first_mailbox().unwrap();
        let trash_refresh = mailbox_reply(3, 1, &[1]);
        assert_eq!(
            session.issue_undo(50).unwrap(),
            MutationRequest::new(
                request_id(4),
                Generation::new(1),
                MessageMutation::undo_trash(undo_token_for_test(11)),
            )
        );
        assert!(session.mutation_request_pending());
        assert!(matches!(
            session.complete_mutation(&mutation_reply(
                4,
                1,
                MutationOutcome::Restored {
                    state: message_state(1, true, false),
                    stats_delta: stats_delta(),
                },
            )),
            MutationCompletion::Applied { .. }
        ));

        assert!(session.match_mailbox_reply(&trash_refresh).is_none());
        assert!(session.mutation_blocks_actions());
        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(5, 1, &[1]));
        assert!(!session.mutation_blocks_actions());
    }

    #[test]
    fn failed_undo_preserves_the_original_refresh_and_retry_slot() {
        let mut session = session_with_visible(&[1]);
        session
            .issue_message_action(EntityKey::new(1).unwrap(), MailAction::Delete)
            .unwrap();
        session.complete_mutation(&mutation_reply(2, 1, moved_to_trash(1, 11, 100)));
        session.issue_first_mailbox().unwrap();
        let trash_refresh = mailbox_reply(3, 1, &[1]);

        session.issue_undo(50).unwrap();
        assert!(matches!(
            session.complete_mutation(&mutation_failure(4, 1, FailureKind::Database)),
            MutationCompletion::Failed { .. }
        ));
        commit_reply(&mut session, &trash_refresh);
        assert!(!session.mutation_blocks_actions());
        assert!(matches!(
            session.issue_undo(60).unwrap(),
            MutationRequest { .. }
        ));
    }

    #[test]
    fn undo_conflict_clears_the_token_but_database_failure_preserves_it() {
        let mut session = session_with_visible(&[1]);
        session
            .issue_message_action(EntityKey::new(1).unwrap(), MailAction::Delete)
            .unwrap();
        session.complete_mutation(&mutation_reply(2, 1, moved_to_trash(1, 11, 100)));
        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(3, 1, &[1]));
        session.issue_undo(50).unwrap();
        assert!(matches!(
            session.complete_mutation(&mutation_failure(4, 1, FailureKind::Database)),
            MutationCompletion::Failed { .. }
        ));
        session.issue_undo(60).unwrap();
        assert!(matches!(
            session.complete_mutation(&mutation_failure(5, 1, FailureKind::Conflict)),
            MutationCompletion::Failed { .. }
        ));
        assert_eq!(session.issue_undo(60), Err(SessionError::UndoUnavailable));
    }

    #[test]
    fn newer_trash_receipt_replaces_the_slot_and_successful_undo_clears_it() {
        let mut session = session_with_visible(&[1]);
        session
            .issue_message_action(EntityKey::new(1).unwrap(), MailAction::Delete)
            .unwrap();
        session.complete_mutation(&mutation_reply(2, 1, moved_to_trash(1, 11, 100)));
        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(3, 1, &[1]));

        session
            .issue_message_action(EntityKey::new(1).unwrap(), MailAction::Delete)
            .unwrap();
        session.complete_mutation(&mutation_reply(4, 1, moved_to_trash(1, 22, 200)));
        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(5, 1, &[1]));
        assert_eq!(
            session.issue_undo(150).unwrap(),
            MutationRequest::new(
                request_id(6),
                Generation::new(1),
                MessageMutation::undo_trash(undo_token_for_test(22)),
            )
        );
        assert!(matches!(
            session.complete_mutation(&mutation_reply(
                6,
                1,
                MutationOutcome::Restored {
                    state: message_state(1, true, false),
                    stats_delta: stats_delta(),
                },
            )),
            MutationCompletion::Applied { .. }
        ));
        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(7, 1, &[1]));
        assert_eq!(session.issue_undo(150), Err(SessionError::UndoUnavailable));
    }

    #[test]
    fn delete_action_uses_the_rust_session_folder_and_request_exhaustion_is_atomic() {
        let mut session = session_with_visible(&[1]);
        assert!(session.set_folder("Trash").unwrap());
        session.issue_first_mailbox().unwrap();
        commit_reply(&mut session, &mailbox_reply(2, 2, &[1]));
        assert_eq!(
            session
                .issue_message_action(EntityKey::new(1).unwrap(), MailAction::Delete)
                .unwrap(),
            MutationRequest::new(
                request_id(3),
                Generation::new(2),
                MessageMutation::delete_permanently(MessageId::new(1).unwrap()),
            )
        );
        session.cancel_mutation_submission();

        session.next_request_id = None;
        assert_eq!(
            session.issue_message_action(EntityKey::new(1).unwrap(), MailAction::Archive),
            Err(SessionError::RequestIdExhausted)
        );
        assert!(!session.mutation_blocks_actions());
    }

    #[test]
    fn navigation_requires_a_committed_boundary_cursor() {
        let mut session = ReadSession::new();
        assert_eq!(
            session.issue_more_mailbox(),
            Err(SessionError::NavigationUnavailable)
        );
    }
}
