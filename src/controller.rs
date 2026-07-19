mod session;

use self::session::{DetailAcceptance, ReadSession, SessionError};
use crate::core::{
    AccountDirectoryLoadError, CoreHandle, Event, EventReceiver, MailboxLoadError,
    MessageLoadError, SubmitError,
};
use crate::presentation::show_snackbar;
use crate::presentation::sqlite::{
    AccountCatalog, ProjectedMailbox, ProjectedMailboxStats, ProjectionError,
};
use crate::store::sqlite::{
    AccountDirectory, DbFailure, FailureKind, MailboxPage, MessageDetail, Tagged,
};
use crate::ui_identity::{AccountKey, EntityKey};
use crate::{AccountItem, AppWindow, MailDetail, MailSummary};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::{cell::RefCell, rc::Rc, time::Duration};

pub(crate) fn install(
    ui: &AppWindow,
    core: CoreHandle,
    mut core_events: EventReceiver,
) -> Result<slint::JoinHandle<()>, slint::EventLoopError> {
    let controller = Rc::new(Controller::new(ui, core));
    controller.install_handlers(ui);
    controller.start();

    let ui_weak = ui.as_weak();
    slint::spawn_local(async move {
        while let Some(event) = core_events.recv().await {
            if ui_weak.upgrade().is_none() {
                return;
            }
            controller.handle_event(event);
        }
        controller.core_closed();
    })
}

struct Controller {
    ui: slint::Weak<AppWindow>,
    core: CoreHandle,
    state: RefCell<ReadSession>,
    catalog: RefCell<Option<AccountCatalog>>,
    pending_mailbox: RefCell<Option<MailboxPage>>,
    mail_model: Rc<VecModel<MailSummary>>,
    account_model: Rc<VecModel<AccountItem>>,
    search_timer: Rc<Timer>,
    snackbar_timer: Rc<Timer>,
}

impl Controller {
    fn new(ui: &AppWindow, core: CoreHandle) -> Self {
        let mail_model = Rc::new(VecModel::from(Vec::<MailSummary>::new()));
        let account_model = Rc::new(VecModel::from(Vec::<AccountItem>::new()));
        ui.set_mails(ModelRc::from(mail_model.clone()));
        ui.set_accounts(ModelRc::from(account_model.clone()));
        ui.set_has_accounts(false);
        ui.set_mail_actions_enabled(false);
        ui.set_initial_loading(true);
        ui.set_mailbox_loading(true);
        ui.set_mailbox_error(false);
        ui.set_detail_loading(false);
        ui.set_detail_error(false);
        ui.set_total_known(true);
        ui.set_data_source_label("SQLite cache".into());
        ui.set_status_text("Loading local cache".into());
        ui.set_selected_id(SharedString::default());
        ui.set_selected_mail(MailDetail::default());

        Self {
            ui: ui.as_weak(),
            core,
            state: RefCell::new(ReadSession::new()),
            catalog: RefCell::new(None),
            pending_mailbox: RefCell::new(None),
            mail_model,
            account_model,
            search_timer: Rc::new(Timer::default()),
            snackbar_timer: Rc::new(Timer::default()),
        }
    }

    fn install_handlers(self: &Rc<Self>, ui: &AppWindow) {
        let controller = self.clone();
        ui.on_select_mail(move |key| controller.select_message(key));

        let controller = self.clone();
        ui.on_filter_folder(move |folder| controller.change_folder(folder.as_str()));

        let controller = self.clone();
        ui.on_query_mail(move |query| {
            let search_timer = controller.search_timer.clone();
            let controller = Rc::downgrade(&controller);
            search_timer.start(
                TimerMode::SingleShot,
                Duration::from_millis(180),
                move || {
                    if let Some(controller) = controller.upgrade() {
                        controller.change_search(query.as_str());
                    }
                },
            );
        });

        let controller = self.clone();
        ui.on_switch_account(move |key| controller.change_account(key.as_str()));

        let controller = self.clone();
        ui.on_retry_mailbox(move || controller.retry_mailbox());

        let controller = self.clone();
        ui.on_retry_detail(move || controller.retry_detail());
    }

    fn start(&self) {
        self.reload_mailbox();
        self.issue_accounts();
    }

    fn issue_accounts(&self) {
        let query = match self.state.borrow_mut().issue_accounts() {
            Ok(query) => query,
            Err(error) => {
                self.fail_mailbox(session_error(error));
                return;
            }
        };
        if let Err(error) = self.core.try_query_account_directory(query) {
            self.state.borrow_mut().cancel_accounts_submission();
            self.fail_accounts(submit_error(error));
        }
    }

    fn reload_mailbox(&self) {
        self.mail_model.set_vec(Vec::new());
        self.pending_mailbox.borrow_mut().take();
        self.clear_reader();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_loading(true);
            ui.set_mailbox_error(false);
            ui.set_status_text("Loading local cache".into());
        }

        let query = match self.state.borrow_mut().issue_first_mailbox() {
            Ok(query) => query,
            Err(error) => {
                self.fail_mailbox(session_error(error));
                return;
            }
        };
        if let Err(error) = self.core.try_query_mailbox(query) {
            self.state.borrow_mut().cancel_mailbox_submission();
            self.fail_mailbox(submit_error(error));
        }
    }

    fn handle_event(&self, event: Event) {
        match event {
            Event::AccountsLoaded(reply) => self.handle_accounts(reply),
            Event::AccountsLoadRejected {
                request_id,
                generation,
                reason,
            } => {
                if self
                    .state
                    .borrow_mut()
                    .accept_accounts(request_id, generation)
                {
                    self.fail_accounts(account_rejection(reason));
                }
            }
            Event::MailboxLoaded(reply) => self.handle_mailbox(reply),
            Event::MailboxLoadRejected {
                request_id,
                generation,
                reason,
            } => {
                if self
                    .state
                    .borrow_mut()
                    .reject_mailbox(request_id, generation)
                {
                    self.fail_mailbox(mailbox_rejection(reason));
                }
            }
            Event::MessageLoaded(reply) => self.handle_detail(reply),
            Event::MessageLoadRejected {
                request_id,
                generation,
                reason,
            } => {
                if self
                    .state
                    .borrow_mut()
                    .reject_message(request_id, generation)
                {
                    self.fail_detail(message_rejection(reason));
                }
            }
            Event::MutationFinished(_) | Event::MutationRejected { .. } => {}
        }
    }

    fn handle_accounts(&self, reply: Tagged<AccountDirectory>) {
        if !self
            .state
            .borrow_mut()
            .accept_accounts(reply.request_id, reply.generation)
        {
            return;
        }
        let directory = match reply.result {
            Ok(directory) => directory,
            Err(failure) => {
                self.fail_accounts(database_error(&failure));
                return;
            }
        };
        let catalog = match AccountCatalog::try_from_directory(directory) {
            Ok(catalog) => catalog,
            Err(error) => {
                self.fail_accounts(projection_error(&error));
                return;
            }
        };

        let active_account = self.state.borrow().account();
        let scope_reset = if catalog.contains(active_account) {
            false
        } else {
            match self.state.borrow_mut().set_account(AccountKey::All) {
                Ok(changed) => changed,
                Err(error) => {
                    self.fail_accounts(session_error(error));
                    return;
                }
            }
        };
        self.account_model.set_vec(catalog.account_items());
        let has_accounts = catalog.len() > 0;
        *self.catalog.borrow_mut() = Some(catalog);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_has_accounts(has_accounts);
            ui.set_initial_loading(false);
        }
        self.refresh_active_account();

        let pending_page = self.pending_mailbox.borrow_mut().take();
        if scope_reset {
            self.reload_mailbox();
        } else if let Some(page) = pending_page {
            self.apply_mailbox(page);
        }
    }

    fn handle_mailbox(&self, reply: Tagged<MailboxPage>) {
        if !self.state.borrow_mut().accept_mailbox(&reply) {
            return;
        }
        let page = match reply.result {
            Ok(page) => page,
            Err(failure) => {
                self.fail_mailbox(database_error(&failure));
                return;
            }
        };
        if self.catalog.borrow().is_none() || self.state.borrow().accounts_pending() {
            *self.pending_mailbox.borrow_mut() = Some(page);
            return;
        }
        self.apply_mailbox(page);
    }

    fn apply_mailbox(&self, page: MailboxPage) {
        let projected = {
            let catalog = self.catalog.borrow();
            let Some(catalog) = catalog.as_ref() else {
                *self.pending_mailbox.borrow_mut() = Some(page);
                return;
            };
            match catalog.project_mailbox(page) {
                Ok(projected) => projected,
                Err(error) => {
                    self.fail_mailbox(projection_error(&error));
                    return;
                }
            }
        };

        let ProjectedMailbox {
            rows,
            stats,
            previous_cursor,
            next_cursor,
        } = projected;
        let row_count = rows.len();
        let has_more = next_cursor.is_some();
        debug_assert!(
            previous_cursor.is_none(),
            "the read-only controller only requests the first mailbox page"
        );
        debug_assert_eq!(has_more, self.state.borrow().next_cursor().is_some());
        self.mail_model.set_vec(rows);
        self.apply_stats(stats);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_loading(false);
            ui.set_mailbox_error(false);
            ui.set_initial_loading(false);
            ui.set_status_text(
                if has_more {
                    "More cached messages available"
                } else if row_count == 0 {
                    "Local cache is empty"
                } else {
                    "Local cache ready"
                }
                .into(),
            );
        }
    }

    fn apply_stats(&self, stats: ProjectedMailboxStats) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_total_known(stats.selected_total.is_some());
            ui.set_message_total(stats.selected_total.unwrap_or_default());
            ui.set_inbox_count(stats.inbox_unread);
            ui.set_starred_count(stats.starred_total);
            ui.set_draft_count(stats.drafts_total);
        }

        if let Some(mut all) = self.account_model.row_data(0)
            && all.unread_count != stats.all_inbox_unread
        {
            all.unread_count = stats.all_inbox_unread;
            self.account_model.set_row_data(0, all);
        }
        for update in stats.account_unread {
            let Some(index) = (1..self.account_model.row_count()).find(|index| {
                self.account_model
                    .row_data(*index)
                    .is_some_and(|account| account.id == update.account_id)
            }) else {
                continue;
            };
            let Some(mut account) = self.account_model.row_data(index) else {
                continue;
            };
            if account.unread_count != update.unread_count {
                account.unread_count = update.unread_count;
                self.account_model.set_row_data(index, account);
            }
        }
    }

    fn handle_detail(&self, reply: Tagged<Option<MessageDetail>>) {
        let acceptance = self.state.borrow_mut().accept_message(&reply);
        match acceptance {
            DetailAcceptance::Stale => {}
            DetailAcceptance::NotFound => {
                self.clear_reader();
                if let Some(ui) = self.ui.upgrade() {
                    ui.set_detail_open(false);
                    ui.set_status_text("Message is no longer available".into());
                    show_snackbar(
                        &ui,
                        "Message is no longer available; the mailbox will refresh",
                        false,
                        &self.snackbar_timer,
                    );
                }
                self.reload_mailbox();
            }
            DetailAcceptance::Failed => {
                if let Err(failure) = &reply.result {
                    self.fail_detail(database_error(failure));
                } else {
                    self.fail_detail(UserError::mail_data());
                }
            }
            DetailAcceptance::Ready => {
                let Ok(Some(detail)) = reply.result else {
                    self.fail_detail(UserError::mail_data());
                    return;
                };
                let folder = self.state.borrow().folder_label();
                let projected = {
                    let catalog = self.catalog.borrow();
                    let Some(catalog) = catalog.as_ref() else {
                        self.fail_detail(UserError::mail_data());
                        return;
                    };
                    match catalog.project_detail(detail, folder) {
                        Ok(projected) => projected,
                        Err(error) => {
                            self.fail_detail(projection_error(&error));
                            return;
                        }
                    }
                };
                if let Some(ui) = self.ui.upgrade() {
                    ui.set_selected_mail(projected);
                    ui.set_detail_loading(false);
                    ui.set_detail_error(false);
                    ui.set_status_text("Message loaded from local cache".into());
                }
            }
        }
    }

    fn select_message(&self, key: SharedString) {
        let Some(id) = EntityKey::parse(key.as_str()) else {
            self.reject_selection("This message can no longer be identified");
            return;
        };
        let query = match self.state.borrow_mut().select_message(id) {
            Ok(query) => query,
            Err(SessionError::MessageNotVisible | SessionError::InvalidIdentity) => {
                self.reject_selection("This message is no longer in the current page");
                return;
            }
            Err(error) => {
                self.fail_detail(session_error(error));
                return;
            }
        };
        if let Some(ui) = self.ui.upgrade() {
            ui.set_selected_id(key);
            ui.set_selected_mail(MailDetail::default());
            ui.set_detail_loading(true);
            ui.set_detail_error(false);
            ui.set_status_text("Loading message".into());
        }
        if let Err(error) = self.core.try_open_message(query) {
            self.state.borrow_mut().cancel_detail_submission();
            self.fail_detail(submit_error(error));
        }
    }

    fn change_folder(&self, folder: &str) {
        match self.state.borrow_mut().set_folder(folder) {
            Ok(false) => return,
            Ok(true) => {}
            Err(error) => {
                self.notify_error(session_error(error));
                return;
            }
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_active_folder(self.state.borrow().folder_label().into());
            ui.set_detail_open(false);
        }
        self.reload_mailbox();
    }

    fn change_search(&self, query: &str) {
        let result = self.state.borrow_mut().set_search(query);
        match result {
            Ok(false) => self.restore_search_text(),
            Ok(true) => {
                self.restore_search_text();
                self.reload_mailbox();
            }
            Err(error) => {
                self.restore_search_text();
                self.notify_error(session_error(error));
            }
        }
    }

    fn restore_search_text(&self) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_search_query(self.state.borrow().search().into());
        }
    }

    fn change_account(&self, key: &str) {
        let Some(account) = AccountKey::parse(key) else {
            self.notify_error(UserError::selection());
            return;
        };
        if !self
            .catalog
            .borrow()
            .as_ref()
            .is_some_and(|catalog| catalog.contains(account))
        {
            self.notify_error(UserError::selection());
            return;
        }
        match self.state.borrow_mut().set_account(account) {
            Ok(false) => return,
            Ok(true) => {}
            Err(error) => {
                self.notify_error(session_error(error));
                return;
            }
        }
        self.refresh_active_account();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_detail_open(false);
        }
        self.reload_mailbox();
    }

    fn refresh_active_account(&self) {
        let account_key = self.state.borrow().account();
        let item = self
            .catalog
            .borrow()
            .as_ref()
            .and_then(|catalog| catalog.active_item(account_key));
        let Some(item) = item else {
            self.notify_error(UserError::selection());
            return;
        };
        if let Some(ui) = self.ui.upgrade() {
            ui.set_active_account_id(item.id);
            ui.set_active_account_name(item.name);
            ui.set_active_account_detail(item.address);
            ui.set_active_account_initials(item.initials);
            ui.set_active_account_color(item.avatar_color);
            ui.set_active_account_error(item.has_error);
        }
    }

    fn retry_mailbox(&self) {
        let needs_accounts = self.catalog.borrow().is_none();
        if needs_accounts && let Some(ui) = self.ui.upgrade() {
            ui.set_initial_loading(true);
            ui.set_mailbox_error(false);
        }
        self.reload_mailbox();
        self.issue_accounts();
    }

    fn retry_detail(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let key = ui.get_selected_id();
        if key.is_empty() {
            ui.set_detail_error(false);
            ui.set_detail_open(false);
            return;
        }
        self.select_message(key);
    }

    fn clear_reader(&self) {
        self.state.borrow_mut().clear_selection();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_selected_id(SharedString::default());
            ui.set_selected_mail(MailDetail::default());
            ui.set_detail_loading(false);
            ui.set_detail_error(false);
            ui.set_detail_open(false);
        }
    }

    fn reject_selection(&self, message: &'static str) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_detail_open(false);
            ui.set_detail_loading(false);
            ui.set_status_text("Message selection changed".into());
            show_snackbar(&ui, message, false, &self.snackbar_timer);
        }
    }

    fn fail_accounts(&self, error: UserError) {
        self.account_model.set_vec(Vec::new());
        self.catalog.borrow_mut().take();
        self.pending_mailbox.borrow_mut().take();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_has_accounts(false);
            ui.set_initial_loading(false);
        }
        self.fail_mailbox(error);
    }

    fn fail_mailbox(&self, error: UserError) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_loading(false);
            ui.set_mailbox_error(true);
            ui.set_mailbox_error_title(error.title.into());
            ui.set_mailbox_error_detail(error.detail.into());
            ui.set_status_text(error.title.into());
        }
    }

    fn fail_detail(&self, error: UserError) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_detail_loading(false);
            ui.set_detail_error(true);
            ui.set_detail_error_title(error.title.into());
            ui.set_detail_error_detail(error.detail.into());
            ui.set_status_text(error.title.into());
        }
    }

    fn core_closed(&self) {
        self.state.borrow_mut().cancel_pending();
        self.pending_mailbox.borrow_mut().take();
        let show_detail_error = self.ui.upgrade().is_some_and(|ui| {
            ui.set_initial_loading(false);
            ui.get_detail_loading() || !ui.get_selected_id().is_empty()
        });
        let error = submit_error(SubmitError::Closed);
        self.fail_mailbox(error);
        if show_detail_error {
            self.fail_detail(error);
        }
    }

    fn notify_error(&self, error: UserError) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_status_text(error.title.into());
            show_snackbar(&ui, error.detail, false, &self.snackbar_timer);
        }
    }
}

#[derive(Clone, Copy)]
struct UserError {
    title: &'static str,
    detail: &'static str,
}

impl UserError {
    const fn mail_data() -> Self {
        Self {
            title: "Mail data could not be displayed",
            detail: "Stored mail metadata is inconsistent or exceeds a safety limit. Try again after checking the local data.",
        }
    }

    const fn selection() -> Self {
        Self {
            title: "Selection is no longer available",
            detail: "Refresh the mailbox and choose the item again.",
        }
    }
}

fn database_error(failure: &DbFailure) -> UserError {
    match failure.kind {
        FailureKind::Database => UserError {
            title: "Local mail is unavailable",
            detail: "Nivalis could not read the local mail database. Check storage permissions and try again.",
        },
        FailureKind::Migration => UserError {
            title: "Mail data could not be upgraded",
            detail: "Keep the local data unchanged and restart with a compatible Nivalis version.",
        },
        FailureKind::ResourceLimit => UserError {
            title: "Mail data exceeds a safety limit",
            detail: "Reduce the selected account, folder, or search scope and try again.",
        },
        FailureKind::NotFound => UserError {
            title: "Mail is no longer available",
            detail: "The account or message changed. Refresh the mailbox and try again.",
        },
        FailureKind::Conflict => UserError {
            title: "Local mail needs attention",
            detail: "Stored mailbox state is inconsistent. Retry after the local cache has been repaired.",
        },
    }
}

fn projection_error(_error: &ProjectionError) -> UserError {
    UserError::mail_data()
}

fn session_error(error: SessionError) -> UserError {
    match error {
        SessionError::InvalidSearch => UserError {
            title: "Search is too long",
            detail: "Shorten the search to 256 UTF-8 bytes and try again.",
        },
        SessionError::InvalidIdentity
        | SessionError::InvalidFolder
        | SessionError::MessageNotVisible => UserError::selection(),
        SessionError::RequestIdExhausted | SessionError::GenerationExhausted => UserError {
            title: "Mail session reached a safety limit",
            detail: "Restart Nivalis before continuing.",
        },
    }
}

fn submit_error(error: SubmitError) -> UserError {
    match error {
        SubmitError::Busy => UserError {
            title: "Local mail is busy",
            detail: "Another local operation is still running. Wait briefly and try again.",
        },
        SubmitError::Closed => UserError {
            title: "Local mail service stopped",
            detail: "Restart Nivalis to reopen the local mail database.",
        },
    }
}

fn account_rejection(error: AccountDirectoryLoadError) -> UserError {
    match error {
        AccountDirectoryLoadError::Busy => submit_error(SubmitError::Busy),
        AccountDirectoryLoadError::Unavailable => submit_error(SubmitError::Closed),
    }
}

fn mailbox_rejection(error: MailboxLoadError) -> UserError {
    match error {
        MailboxLoadError::Busy => submit_error(SubmitError::Busy),
        MailboxLoadError::Unavailable => submit_error(SubmitError::Closed),
    }
}

fn message_rejection(error: MessageLoadError) -> UserError {
    match error {
        MessageLoadError::Busy => submit_error(SubmitError::Busy),
        MessageLoadError::Unavailable => submit_error(SubmitError::Closed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_failures_have_actionable_stable_messages() {
        for kind in [
            FailureKind::Database,
            FailureKind::Migration,
            FailureKind::ResourceLimit,
            FailureKind::NotFound,
            FailureKind::Conflict,
        ] {
            let error = database_error(&DbFailure {
                kind,
                message: "internal details must not reach the UI".into(),
            });
            assert!(!error.title.is_empty());
            assert!(!error.detail.is_empty());
            assert!(!error.detail.contains("internal details"));
        }
    }
}
