mod session;

use self::session::{
    DetailAcceptance, MailAction, MailboxAcceptance, MailboxIntent, MutationCompletion,
    MutationIntent, MutationScope, ReadSession, SessionError,
};
use crate::core::{
    AccountConfigDraft, AccountDirectoryLoadError, AccountOperation, AccountOperationFailure,
    AccountOperationResponseError, AccountOperationSubmitError, AccountOperationSuccess,
    AccountSetupMode, AccountWorkflowFailureKind, CoreHandle, Event, EventReceiver,
    InboxSyncFailureKind, MailboxLoadError, MessageLoadError, MutationSubmitError, RequestId,
    SubmitError,
};
use crate::credentials::Secret;
use crate::presentation::sqlite::{
    AccountCatalog, AccountOperationTarget, ProjectedMailbox, ProjectedMailboxStats,
    ProjectionError,
};
use crate::presentation::{show_snackbar, show_snackbar_for};
use crate::store::sqlite::{
    AccountDiagnosticKind, AccountDirectory, AccountValidationError, DbFailure, FailureKind,
    MailboxPage, MessageDetail, Tagged,
};
use crate::ui_identity::{AccountKey, EntityKey};
use crate::{AccountItem, AppWindow, MailDetail, MailSummary};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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
    pending_mailbox: RefCell<Option<PendingMailboxProjection>>,
    mail_model: Rc<VecModel<MailSummary>>,
    account_model: Rc<VecModel<AccountItem>>,
    account_task: RefCell<Option<slint::JoinHandle<()>>>,
    account_request_id: Cell<u64>,
    account_cancelable: Cell<bool>,
    pending_account_selection: Cell<Option<i64>>,
    search_timer: Rc<Timer>,
    snackbar_timer: Rc<Timer>,
}

struct PendingMailboxProjection {
    acceptance: MailboxAcceptance,
    page: MailboxPage,
}

impl Controller {
    fn new(ui: &AppWindow, core: CoreHandle) -> Self {
        let mail_model = Rc::new(VecModel::from(Vec::<MailSummary>::new()));
        let account_model = Rc::new(VecModel::from(Vec::<AccountItem>::new()));
        ui.set_mails(ModelRc::from(mail_model.clone()));
        ui.set_accounts(ModelRc::from(account_model.clone()));
        ui.set_has_accounts(false);
        ui.set_mail_actions_enabled(false);
        ui.set_mutation_loading(false);
        ui.set_undo_loading(false);
        ui.set_initial_loading(true);
        ui.set_mailbox_loading(true);
        ui.set_has_previous_mailbox_page(false);
        ui.set_has_next_mailbox_page(false);
        ui.set_mailbox_navigation_loading(false);
        ui.set_mailbox_page_number(1);
        ui.set_mailbox_error(false);
        ui.set_detail_loading(false);
        ui.set_detail_error(false);
        ui.set_total_known(true);
        ui.set_data_source_label("SQLite cache".into());
        ui.set_status_text("Loading local cache".into());
        ui.set_account_operation_loading(false);
        ui.set_sync_loading(false);
        ui.set_account_operation_stage(SharedString::default());
        ui.set_account_operation_error(SharedString::default());
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
            account_task: RefCell::new(None),
            account_request_id: Cell::new(1),
            account_cancelable: Cell::new(false),
            pending_account_selection: Cell::new(None),
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
        ui.on_manage_account(move |key| controller.manage_account(key.as_str()));

        let controller = self.clone();
        ui.on_add_account(move |name, address, login, host, port, password| {
            controller.add_account(name, address, login, host, port, password);
        });

        let controller = self.clone();
        ui.on_diagnose_account(move |key| controller.diagnose_account(key.as_str()));

        let controller = self.clone();
        ui.on_sync_account(move |key| controller.sync_account(key.as_str()));

        let controller = self.clone();
        ui.on_remove_account(move |key| controller.remove_account(key.as_str()));

        let controller = self.clone();
        ui.on_cancel_account_operation(move || controller.cancel_account_operation());

        let controller = self.clone();
        ui.on_retry_mailbox(move || controller.retry_mailbox());

        let controller = self.clone();
        ui.on_previous_mailbox_page(move || controller.navigate_previous());

        let controller = self.clone();
        ui.on_next_mailbox_page(move || controller.navigate_next());

        let controller = self.clone();
        ui.on_retry_detail(move || controller.retry_detail());

        let controller = self.clone();
        ui.on_toggle_star(move |key| controller.toggle_star(key));

        let controller = self.clone();
        ui.on_archive(move |key| controller.archive_message(key));

        let controller = self.clone();
        ui.on_delete_mail(move |key| controller.delete_message(key));

        let controller = self.clone();
        ui.on_mark_unread(move |key| controller.mark_unread(key));

        let controller = self.clone();
        ui.on_undo_delete(move || controller.undo_delete());
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
            ui.set_has_previous_mailbox_page(false);
            ui.set_has_next_mailbox_page(false);
            ui.set_mailbox_navigation_loading(false);
            ui.set_mailbox_page_number(1);
            ui.set_mailbox_error(false);
            ui.set_mail_actions_enabled(false);
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
                if let Some(intent) = self
                    .state
                    .borrow_mut()
                    .reject_mailbox(request_id, generation)
                {
                    self.fail_mailbox_request(intent, mailbox_rejection(reason));
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
            Event::MutationFinished(reply) => self.handle_mutation(reply),
            Event::MutationRejected {
                request_id,
                generation,
                reason,
            } => self.handle_mutation_rejection(request_id, generation, reason),
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

        let requested_account = self
            .pending_account_selection
            .take()
            .and_then(EntityKey::new)
            .map(AccountKey::Account)
            .filter(|account| catalog.contains(*account));
        let requested_change = if let Some(account) = requested_account {
            match self.state.borrow_mut().set_account(account) {
                Ok(changed) => changed,
                Err(error) => {
                    self.fail_accounts(session_error(error));
                    return;
                }
            }
        } else {
            false
        };
        let active_account = self.state.borrow().account();
        let scope_reset = if catalog.contains(active_account) {
            requested_change
        } else {
            match self.state.borrow_mut().set_account(AccountKey::All) {
                Ok(changed) => requested_change || changed,
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
        self.refresh_managed_account();

        let pending_page = self.pending_mailbox.borrow_mut().take();
        if scope_reset {
            self.reload_mailbox();
        } else if let Some(pending) = pending_page {
            self.apply_mailbox(pending.acceptance, pending.page);
        }
    }

    fn handle_mailbox(&self, reply: Tagged<MailboxPage>) {
        let Some(acceptance) = self.state.borrow_mut().match_mailbox_reply(&reply) else {
            return;
        };
        let page = match reply.result {
            Ok(page) => page,
            Err(failure) => {
                self.fail_mailbox_request(acceptance.intent(), database_error(&failure));
                return;
            }
        };
        if self.catalog.borrow().is_none() || self.state.borrow().accounts_pending() {
            *self.pending_mailbox.borrow_mut() =
                Some(PendingMailboxProjection { acceptance, page });
            return;
        }
        self.apply_mailbox(acceptance, page);
    }

    fn apply_mailbox(&self, acceptance: MailboxAcceptance, page: MailboxPage) {
        let commit = match acceptance.stage(&page) {
            Ok(commit) => commit,
            Err(SessionError::EmptyNavigationPage) => {
                self.recover_changed_mailbox();
                return;
            }
            Err(error) => {
                self.fail_mailbox_request(acceptance.intent(), session_error(error));
                return;
            }
        };
        let projected = {
            let catalog = self.catalog.borrow();
            let Some(catalog) = catalog.as_ref() else {
                *self.pending_mailbox.borrow_mut() =
                    Some(PendingMailboxProjection { acceptance, page });
                return;
            };
            match catalog.project_mailbox(page) {
                Ok(projected) => projected,
                Err(error) => {
                    self.fail_mailbox_request(acceptance.intent(), projection_error(&error));
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
        let (committed, replacement_pending) = {
            let mut state = self.state.borrow_mut();
            let committed = state.commit_mailbox(commit);
            (committed, state.mailbox_pending())
        };
        if !committed {
            if !replacement_pending {
                self.fail_mailbox_request(acceptance.intent(), UserError::mailbox_changed());
            }
            return;
        }
        let state = self.state.borrow();
        debug_assert_eq!(previous_cursor, state.previous_cursor());
        debug_assert_eq!(next_cursor, state.next_cursor());
        let has_previous = state.previous_cursor().is_some();
        let has_next = state.next_cursor().is_some();
        let page_number = i32::try_from(state.page_number())
            .expect("session page numbers are bounded to the Slint integer range");
        drop(state);

        let selected_to_restore = self.selected_reader_key();
        self.clear_reader();
        self.mail_model.set_vec(rows);
        self.apply_stats(stats);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_loading(false);
            ui.set_has_previous_mailbox_page(has_previous);
            ui.set_has_next_mailbox_page(has_next);
            ui.set_mailbox_navigation_loading(false);
            ui.set_mailbox_page_number(page_number);
            ui.set_mailbox_error(false);
            ui.set_initial_loading(false);
            ui.set_status_text(
                if page_number > 1 {
                    format!("Page {page_number} loaded from local cache")
                } else if has_next {
                    "More cached messages available".to_owned()
                } else if row_count == 0 {
                    "Local cache is empty".to_owned()
                } else {
                    "Local cache ready".to_owned()
                }
                .into(),
            );
        }
        self.sync_mutation_ui();
        if let Some(key) = selected_to_restore
            && visible_summary_state(self.mail_model.as_ref(), key).is_some()
        {
            if let Some(ui) = self.ui.upgrade() {
                ui.set_detail_open(true);
            }
            self.select_message(key.encode());
        }
    }

    fn navigate_next(&self) {
        self.submit_mailbox_navigation(MailboxIntent::Next);
    }

    fn navigate_previous(&self) {
        self.submit_mailbox_navigation(MailboxIntent::Previous);
    }

    fn submit_mailbox_navigation(&self, intent: MailboxIntent) {
        let query = {
            let mut state = self.state.borrow_mut();
            match intent {
                MailboxIntent::First => state.issue_first_mailbox(),
                MailboxIntent::Next => state.issue_next_mailbox(),
                MailboxIntent::Previous => state.issue_previous_mailbox(),
            }
        };
        let query = match query {
            Ok(query) => query,
            Err(SessionError::MailboxRequestPending | SessionError::NavigationUnavailable) => {
                return;
            }
            Err(error) => {
                self.fail_navigation(session_error(error));
                return;
            }
        };

        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_navigation_loading(true);
            ui.set_mailbox_error(false);
            ui.set_status_text(
                match intent {
                    MailboxIntent::First => "Refreshing the first page",
                    MailboxIntent::Next => "Loading the next page",
                    MailboxIntent::Previous => "Loading the previous page",
                }
                .into(),
            );
        }
        if let Err(error) = self.core.try_query_mailbox(query) {
            self.state.borrow_mut().cancel_mailbox_submission();
            self.fail_navigation(submit_error(error));
        }
    }

    fn recover_changed_mailbox(&self) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_status_text("Mailbox changed; refreshing the first page".into());
            show_snackbar(
                &ui,
                "Mailbox contents changed; returning to the first page",
                false,
                &self.snackbar_timer,
            );
        }
        self.submit_mailbox_navigation(MailboxIntent::First);
    }

    fn fail_mailbox_request(&self, intent: MailboxIntent, error: UserError) {
        let preserves_page = intent != MailboxIntent::First
            || self
                .ui
                .upgrade()
                .is_some_and(|ui| ui.get_mailbox_navigation_loading());
        if preserves_page {
            self.fail_navigation(error);
        } else {
            self.fail_mailbox(error);
        }
    }

    fn fail_navigation(&self, error: UserError) {
        if self.state.borrow().mutation_refresh_pending() {
            self.fail_mailbox(error);
            self.sync_mutation_ui();
            return;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_navigation_loading(false);
            ui.set_status_text(error.title.into());
            show_snackbar(&ui, error.detail, false, &self.snackbar_timer);
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

    fn toggle_star(&self, key: SharedString) {
        let Some((key, summary)) = self.visible_summary(key.as_str()) else {
            self.reject_mutation_selection();
            return;
        };
        self.submit_message_action(key, desired_star_action(summary));
    }

    fn mark_unread(&self, key: SharedString) {
        let Some((key, summary)) = self.visible_summary(key.as_str()) else {
            self.reject_mutation_selection();
            return;
        };
        let Some(action) = desired_unread_action(summary) else {
            if let Some(ui) = self.ui.upgrade() {
                ui.set_status_text("Message is already unread".into());
                show_snackbar(
                    &ui,
                    "Message is already unread",
                    false,
                    &self.snackbar_timer,
                );
            }
            return;
        };
        self.submit_message_action(key, action);
    }

    fn archive_message(&self, key: SharedString) {
        self.submit_keyed_action(key.as_str(), MailAction::Archive);
    }

    fn delete_message(&self, key: SharedString) {
        self.submit_keyed_action(key.as_str(), MailAction::Delete);
    }

    fn submit_keyed_action(&self, key: &str, action: MailAction) {
        let Some(key) = EntityKey::parse(key) else {
            self.reject_mutation_selection();
            return;
        };
        self.submit_message_action(key, action);
    }

    fn submit_message_action(&self, key: EntityKey, action: MailAction) {
        let request = match self.state.borrow_mut().issue_message_action(key, action) {
            Ok(request) => request,
            Err(error) => {
                self.notify_error(session_error(error));
                return;
            }
        };
        self.sync_mutation_ui();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_status_text("Saving message change".into());
        }
        if let Err(error) = self.core.try_mutate(request) {
            let intent = self.state.borrow_mut().cancel_mutation_submission();
            self.sync_mutation_ui();
            if let Some(intent) = intent {
                self.notify_mutation_error(intent, submit_error(error));
            }
        }
    }

    fn undo_delete(&self) {
        let now_ms = match unix_time_ms(SystemTime::now()) {
            Some(now_ms) => now_ms,
            None => {
                self.notify_error(UserError::system_clock());
                return;
            }
        };
        let request = match self.state.borrow_mut().issue_undo(now_ms) {
            Ok(request) => request,
            Err(error) => {
                self.notify_error(session_error(error));
                return;
            }
        };
        self.sync_mutation_ui();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_status_text("Restoring message".into());
        }
        if let Err(error) = self.core.try_mutate(request) {
            let intent = self.state.borrow_mut().cancel_mutation_submission();
            self.sync_mutation_ui();
            if let Some(intent) = intent {
                self.notify_mutation_error(intent, submit_error(error));
            }
        }
    }

    fn visible_summary(&self, key: &str) -> Option<(EntityKey, SummaryState)> {
        let key = EntityKey::parse(key)?;
        visible_summary_state(self.mail_model.as_ref(), key).map(|summary| (key, summary))
    }

    fn reject_mutation_selection(&self) {
        self.notify_error(UserError::selection());
    }

    fn handle_mutation(&self, reply: Tagged<crate::store::sqlite::MutationOutcome>) {
        let failure_kind = reply.result.as_ref().err().map(|failure| failure.kind);
        let failure = reply.result.as_ref().err().map(database_error);
        let completion = self.state.borrow_mut().complete_mutation(&reply);
        match completion {
            MutationCompletion::Stale => {}
            MutationCompletion::Failed { intent, .. } => {
                self.sync_mutation_ui();
                self.notify_mutation_error(
                    intent,
                    failure.unwrap_or_else(UserError::mutation_result),
                );
                if failure_kind == Some(FailureKind::NotFound) {
                    self.reload_mailbox();
                }
            }
            MutationCompletion::Applied { intent, scope } => {
                self.finish_mutation_success(intent, scope);
                self.refresh_after_mutation();
            }
            MutationCompletion::OutcomeMismatch { .. } => {
                self.sync_mutation_ui();
                self.notify_error(UserError::mutation_result());
                self.refresh_after_mutation();
            }
        }
    }

    fn handle_mutation_rejection(
        &self,
        request_id: crate::core::RequestId,
        generation: crate::core::Generation,
        reason: MutationSubmitError,
    ) {
        let Some((intent, _scope)) = self
            .state
            .borrow_mut()
            .reject_mutation(request_id, generation)
        else {
            return;
        };
        self.sync_mutation_ui();
        self.notify_mutation_error(intent, mutation_rejection(reason));
    }

    fn finish_mutation_success(&self, intent: MutationIntent, scope: MutationScope) {
        let feedback = mutation_success_feedback(intent);
        let undo_duration = if matches!(intent, MutationIntent::MoveToTrash { .. }) {
            let now = SystemTime::now();
            unix_time_ms(now).and_then(|now_ms| {
                self.state
                    .borrow_mut()
                    .undo_expires_at_ms(now_ms)
                    .and_then(|deadline| undo_remaining(deadline, now))
            })
        } else {
            None
        };

        if let Some(ui) = self.ui.upgrade() {
            if feedback.close_delete_dialog {
                ui.set_delete_dialog_open(false);
            }
            if feedback.close_reader && scope == MutationScope::Current {
                ui.set_detail_open(false);
            }
            ui.set_status_text(feedback.status.into());
            if let Some(duration) = undo_duration {
                show_snackbar_for(&ui, feedback.snackbar, true, &self.snackbar_timer, duration);
            } else {
                show_snackbar(&ui, feedback.snackbar, false, &self.snackbar_timer);
            }
        }
        self.sync_mutation_ui();
    }

    fn refresh_after_mutation(&self) {
        self.pending_mailbox.borrow_mut().take();
        self.submit_mailbox_navigation(MailboxIntent::First);
    }

    fn notify_mutation_error(&self, intent: MutationIntent, error: UserError) {
        if matches!(intent, MutationIntent::UndoTrash { .. }) && self.show_undo_error(error) {
            return;
        }
        self.notify_error(error);
    }

    fn show_undo_error(&self, error: UserError) -> bool {
        let now = SystemTime::now();
        let Some(duration) = unix_time_ms(now).and_then(|now_ms| {
            self.state
                .borrow_mut()
                .undo_expires_at_ms(now_ms)
                .and_then(|deadline| undo_remaining(deadline, now))
        }) else {
            return false;
        };
        let Some(ui) = self.ui.upgrade() else {
            return true;
        };
        ui.set_status_text(error.title.into());
        show_snackbar_for(&ui, error.detail, true, &self.snackbar_timer, duration);
        true
    }

    fn selected_reader_key(&self) -> Option<EntityKey> {
        let ui = self.ui.upgrade()?;
        ui.get_detail_open()
            .then(|| ui.get_selected_id())
            .and_then(|key| EntityKey::parse(key.as_str()))
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

    fn manage_account(&self, key: &str) {
        let Some(account_key) = AccountKey::parse(key) else {
            self.notify_error(UserError::selection());
            return;
        };
        let catalog = self.catalog.borrow();
        let Some(target) = catalog
            .as_ref()
            .and_then(|catalog| catalog.operation_target(account_key))
        else {
            self.notify_error(UserError::selection());
            return;
        };
        let Some(item) = catalog
            .as_ref()
            .and_then(|catalog| catalog.active_item(account_key))
        else {
            self.notify_error(UserError::selection());
            return;
        };
        drop(catalog);

        if let Some(ui) = self.ui.upgrade() {
            ui.set_managed_account_id(target.account_id.get().to_string().into());
            ui.set_managed_account_name(item.name);
            ui.set_managed_account_address(item.address);
            ui.set_managed_account_status(item.status);
            ui.set_managed_account_has_error(item.has_error);
            ui.set_account_operation_error(SharedString::default());
            ui.set_account_menu_open(false);
            ui.set_account_status_open(true);
        }
    }

    fn add_account(
        self: &Rc<Self>,
        name: SharedString,
        address: SharedString,
        login: SharedString,
        host: SharedString,
        port: SharedString,
        password: SharedString,
    ) {
        let Some(port) = parse_imap_port(port.as_str()) else {
            self.show_account_error(UserError::account_port());
            return;
        };
        let draft = match AccountConfigDraft::new(
            name.as_str(),
            address.as_str(),
            login.as_str(),
            host.as_str(),
            port,
            account_accent(address.as_str()),
        ) {
            Ok(draft) => draft,
            Err(error) => {
                self.show_account_error(account_validation_error(error));
                return;
            }
        };
        let secret = match Secret::new(password.as_bytes().to_vec()) {
            Ok(secret) => secret,
            Err(_) => {
                self.show_account_error(UserError::account_password());
                return;
            }
        };
        let Some(request_id) = self.next_account_request_id() else {
            self.show_account_error(UserError::account_session_limit());
            return;
        };
        if !self.begin_account_task("Saving account securely", false) {
            return;
        }
        let response = match self.core.try_account_operation(AccountOperation::Setup {
            request_id,
            mode: AccountSetupMode::Create,
            draft,
            secret,
        }) {
            Ok(response) => response,
            Err(failure) => {
                self.finish_account_task_with_error(account_submit_error(failure.reason()));
                return;
            }
        };

        let weak = Rc::downgrade(self);
        let task = slint::spawn_local(async move {
            let setup_reply = response.await;
            let Some(controller) = weak.upgrade() else {
                return;
            };
            let target = match setup_reply {
                Ok(reply) if reply.request_id == request_id => match reply.result {
                    Ok(AccountOperationSuccess::Configured {
                        account_id,
                        generation,
                    }) => AccountOperationTarget {
                        account_id,
                        expected_generation: generation,
                    },
                    Ok(_) => {
                        controller.finish_account_task_with_error(UserError::account_result());
                        return;
                    }
                    Err(failure) => {
                        controller.finish_setup_failure(failure, &name, &address);
                        return;
                    }
                },
                Ok(_) => {
                    controller.finish_account_task_with_error(UserError::account_result());
                    return;
                }
                Err(error) => {
                    controller.finish_account_task_with_error(account_response_error(error));
                    return;
                }
            };

            controller
                .pending_account_selection
                .set(Some(target.account_id.get()));
            controller.show_managed_account(target, name, address, "Checking connection", false);
            controller.issue_accounts();
            controller.account_cancelable.set(true);
            if let Some(ui) = controller.ui.upgrade() {
                ui.set_account_setup_open(false);
                ui.set_account_status_open(true);
                ui.set_account_operation_stage("Checking connection".into());
            }
            let Some(diagnostic_request_id) = controller.next_account_request_id() else {
                controller.finish_account_task_with_error(UserError::account_session_limit());
                return;
            };
            let diagnostic =
                match controller
                    .core
                    .try_account_operation(AccountOperation::Diagnose {
                        request_id: diagnostic_request_id,
                        account_id: target.account_id,
                        expected_generation: target.expected_generation,
                    }) {
                    Ok(response) => response,
                    Err(failure) => {
                        controller
                            .finish_account_task_with_error(account_submit_error(failure.reason()));
                        return;
                    }
                };
            drop(controller);

            let result = diagnostic.await;
            if let Some(controller) = weak.upgrade() {
                controller.finish_diagnostic(result, diagnostic_request_id, target);
            }
        });
        self.store_account_task(task);
    }

    fn diagnose_account(self: &Rc<Self>, key: &str) {
        let Some(target) = self.account_operation_target(key) else {
            self.show_account_error(UserError::selection());
            return;
        };
        let Some(request_id) = self.next_account_request_id() else {
            self.show_account_error(UserError::account_session_limit());
            return;
        };
        if !self.begin_account_task("Checking connection", true) {
            return;
        }
        let response = match self.core.try_account_operation(AccountOperation::Diagnose {
            request_id,
            account_id: target.account_id,
            expected_generation: target.expected_generation,
        }) {
            Ok(response) => response,
            Err(failure) => {
                self.finish_account_task_with_error(account_submit_error(failure.reason()));
                return;
            }
        };
        let weak = Rc::downgrade(self);
        let task = slint::spawn_local(async move {
            let result = response.await;
            if let Some(controller) = weak.upgrade() {
                controller.finish_diagnostic(result, request_id, target);
            }
        });
        self.store_account_task(task);
    }

    fn sync_account(self: &Rc<Self>, key: &str) {
        let Some(target) = self.account_operation_target(key) else {
            self.show_sync_error(UserError::selection());
            return;
        };
        let Some(request_id) = self.next_account_request_id() else {
            self.show_sync_error(UserError::account_session_limit());
            return;
        };
        if !self.begin_account_task("Syncing inbox", false) {
            return;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_sync_loading(true);
            ui.set_status_text("Syncing inbox".into());
        }
        let response = match self
            .core
            .try_account_operation(AccountOperation::SyncInbox {
                request_id,
                account_id: target.account_id,
                expected_generation: target.expected_generation,
            }) {
            Ok(response) => response,
            Err(failure) => {
                self.finish_sync_with_error(account_submit_error(failure.reason()));
                return;
            }
        };
        let weak = Rc::downgrade(self);
        let task = slint::spawn_local(async move {
            let result = response.await;
            let Some(controller) = weak.upgrade() else {
                return;
            };
            match result {
                Ok(reply) if reply.request_id == request_id => match reply.result {
                    Ok(AccountOperationSuccess::Synced {
                        account_id,
                        generation,
                        imported,
                        has_more,
                    }) if account_id == target.account_id
                        && generation == target.expected_generation =>
                    {
                        let feedback = sync_success_feedback(imported, has_more);
                        controller.finish_account_task();
                        if let Some(ui) = controller.ui.upgrade() {
                            ui.set_status_text(feedback.clone().into());
                            show_snackbar(&ui, &feedback, false, &controller.snackbar_timer);
                        }
                        controller.issue_accounts();
                        controller.reload_mailbox();
                    }
                    Err(failure) => {
                        controller.finish_sync_with_error(account_operation_error(failure))
                    }
                    Ok(_) => controller.finish_sync_with_error(UserError::account_result()),
                },
                Ok(_) => controller.finish_sync_with_error(UserError::account_result()),
                Err(error) => {
                    controller.finish_sync_with_error(account_response_error(error));
                }
            }
        });
        self.store_account_task(task);
    }

    fn remove_account(self: &Rc<Self>, key: &str) {
        let Some(target) = self.account_operation_target(key) else {
            self.show_account_error(UserError::selection());
            return;
        };
        let Some(request_id) = self.next_account_request_id() else {
            self.show_account_error(UserError::account_session_limit());
            return;
        };
        if !self.begin_account_task("Removing account", false) {
            return;
        }
        let response = match self.core.try_account_operation(AccountOperation::Remove {
            request_id,
            account_id: target.account_id,
            expected_generation: target.expected_generation,
        }) {
            Ok(response) => response,
            Err(failure) => {
                self.finish_account_task_with_error(account_submit_error(failure.reason()));
                return;
            }
        };
        let weak = Rc::downgrade(self);
        let task = slint::spawn_local(async move {
            let result = response.await;
            let Some(controller) = weak.upgrade() else {
                return;
            };
            match result {
                Ok(reply)
                    if reply.request_id == request_id
                        && matches!(
                            reply.result,
                            Ok(AccountOperationSuccess::Removed { account_id })
                                if account_id == target.account_id
                        ) =>
                {
                    controller.finish_account_task();
                    if let Some(ui) = controller.ui.upgrade() {
                        ui.set_account_remove_open(false);
                        ui.set_account_status_open(false);
                        ui.set_status_text("Account removed".into());
                        show_snackbar(
                            &ui,
                            "Account and its local cache were removed",
                            false,
                            &controller.snackbar_timer,
                        );
                    }
                    controller.issue_accounts();
                }
                Ok(reply) if reply.request_id == request_id => match reply.result {
                    Err(failure) => {
                        controller.finish_account_task_with_error(account_operation_error(failure))
                    }
                    Ok(_) => controller.finish_account_task_with_error(UserError::account_result()),
                },
                Ok(_) => {
                    controller.finish_account_task_with_error(UserError::account_result());
                }
                Err(error) => {
                    controller.finish_account_task_with_error(account_response_error(error));
                }
            }
        });
        self.store_account_task(task);
    }

    fn account_operation_target(&self, key: &str) -> Option<AccountOperationTarget> {
        let key = AccountKey::parse(key)?;
        self.catalog.borrow().as_ref()?.operation_target(key)
    }

    fn next_account_request_id(&self) -> Option<RequestId> {
        let current = self.account_request_id.get();
        let next = current.checked_add(1)?;
        let request_id = RequestId::new(current).ok()?;
        self.account_request_id.set(next);
        Some(request_id)
    }

    fn begin_account_task(&self, stage: &'static str, cancelable: bool) -> bool {
        let mut task = self.account_task.borrow_mut();
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            drop(task);
            self.show_account_error(UserError::account_busy());
            return false;
        }
        task.take();
        self.account_cancelable.set(cancelable);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_account_operation_loading(true);
            ui.set_account_operation_stage(stage.into());
            ui.set_account_operation_error(SharedString::default());
        }
        true
    }

    fn store_account_task(&self, task: Result<slint::JoinHandle<()>, slint::EventLoopError>) {
        match task {
            Ok(task) => *self.account_task.borrow_mut() = Some(task),
            Err(_) => self.finish_account_task_with_error(UserError::account_runtime()),
        }
    }

    fn cancel_account_operation(&self) {
        if !self.account_cancelable.replace(false) {
            return;
        }
        if let Some(task) = self.account_task.borrow_mut().take() {
            task.abort();
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_account_operation_loading(false);
            ui.set_account_operation_stage(SharedString::default());
            ui.set_account_operation_error(
                "Connection check cancelled. Run it again when you are ready.".into(),
            );
            ui.set_status_text("Connection check cancelled".into());
        }
        self.issue_accounts();
    }

    fn finish_setup_failure(
        &self,
        failure: AccountOperationFailure,
        name: &SharedString,
        address: &SharedString,
    ) {
        if let (Some(account_id), Some(expected_generation)) =
            (failure.account_id, failure.generation)
        {
            self.show_managed_account(
                AccountOperationTarget {
                    account_id,
                    expected_generation,
                },
                name.clone(),
                address.clone(),
                "Setup needs attention",
                true,
            );
            if let Some(ui) = self.ui.upgrade() {
                ui.set_account_setup_open(false);
                ui.set_account_status_open(true);
            }
            self.issue_accounts();
        }
        self.finish_account_task_with_error(account_operation_error(failure));
    }

    fn finish_diagnostic(
        &self,
        result: Result<crate::core::AccountOperationReply, AccountOperationResponseError>,
        request_id: RequestId,
        target: AccountOperationTarget,
    ) {
        match result {
            Ok(reply)
                if reply.request_id == request_id
                    && matches!(
                        reply.result,
                        Ok(AccountOperationSuccess::Diagnosed {
                            account_id,
                            generation,
                        }) if account_id == target.account_id
                            && generation == target.expected_generation
                    ) =>
            {
                self.finish_account_task();
                if let Some(ui) = self.ui.upgrade() {
                    ui.set_managed_account_status("Connected".into());
                    ui.set_managed_account_has_error(false);
                    ui.set_account_status_open(false);
                    ui.set_status_text("Account connected".into());
                    show_snackbar(&ui, "Account connected", false, &self.snackbar_timer);
                }
                self.issue_accounts();
            }
            Ok(reply) if reply.request_id == request_id => match reply.result {
                Err(failure) => {
                    let error = account_operation_error(failure);
                    if let Some(ui) = self.ui.upgrade() {
                        ui.set_managed_account_status(error.title.into());
                        ui.set_managed_account_has_error(true);
                        ui.set_account_status_open(true);
                    }
                    self.finish_account_task_with_error(error);
                    self.issue_accounts();
                }
                Ok(_) => self.finish_account_task_with_error(UserError::account_result()),
            },
            Ok(_) => self.finish_account_task_with_error(UserError::account_result()),
            Err(error) => self.finish_account_task_with_error(account_response_error(error)),
        }
    }

    fn show_managed_account(
        &self,
        target: AccountOperationTarget,
        name: SharedString,
        address: SharedString,
        status: &'static str,
        has_error: bool,
    ) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_managed_account_id(target.account_id.get().to_string().into());
            ui.set_managed_account_name(name);
            ui.set_managed_account_address(address);
            ui.set_managed_account_status(status.into());
            ui.set_managed_account_has_error(has_error);
        }
    }

    fn finish_account_task(&self) {
        self.account_task.borrow_mut().take();
        self.account_cancelable.set(false);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_account_operation_loading(false);
            ui.set_sync_loading(false);
            ui.set_account_operation_stage(SharedString::default());
        }
    }

    fn finish_account_task_with_error(&self, error: UserError) {
        self.finish_account_task();
        self.show_account_error(error);
    }

    fn finish_sync_with_error(&self, error: UserError) {
        self.finish_account_task();
        self.show_sync_error(error);
    }

    fn show_sync_error(&self, error: UserError) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_account_operation_error(error.detail.into());
            ui.set_status_text(error.title.into());
            show_snackbar(&ui, error.detail, false, &self.snackbar_timer);
        }
    }

    fn show_account_error(&self, error: UserError) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_account_operation_error(error.detail.into());
            ui.set_status_text(error.title.into());
        }
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

    fn refresh_managed_account(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let key = ui.get_managed_account_id();
        if key.is_empty() || ui.get_account_operation_loading() {
            return;
        }
        let Some(account_key) = AccountKey::parse(key.as_str()) else {
            ui.set_account_status_open(false);
            ui.set_account_remove_open(false);
            return;
        };
        let item = self
            .catalog
            .borrow()
            .as_ref()
            .and_then(|catalog| catalog.active_item(account_key));
        let Some(item) = item else {
            ui.set_account_status_open(false);
            ui.set_account_remove_open(false);
            ui.set_managed_account_id(SharedString::default());
            return;
        };
        ui.set_managed_account_name(item.name);
        ui.set_managed_account_address(item.address);
        ui.set_managed_account_status(item.status);
        ui.set_managed_account_has_error(item.has_error);
    }

    fn retry_mailbox(&self) {
        if self.state.borrow().mutation_refresh_pending() {
            self.pending_mailbox.borrow_mut().take();
            self.submit_mailbox_navigation(MailboxIntent::First);
            return;
        }
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
            ui.set_delete_dialog_open(false);
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
            ui.set_mail_actions_enabled(false);
        }
        self.fail_mailbox(error);
    }

    fn fail_mailbox(&self, error: UserError) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_loading(false);
            ui.set_has_previous_mailbox_page(false);
            ui.set_has_next_mailbox_page(false);
            ui.set_mailbox_navigation_loading(false);
            ui.set_mailbox_page_number(1);
            ui.set_mailbox_error(true);
            ui.set_mailbox_error_title(error.title.into());
            ui.set_mailbox_error_detail(error.detail.into());
            ui.set_status_text(error.title.into());
            ui.set_mail_actions_enabled(false);
            ui.set_delete_dialog_open(false);
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
        if let Some(task) = self.account_task.borrow_mut().take() {
            task.abort();
        }
        self.account_cancelable.set(false);
        self.snackbar_timer.stop();
        let show_detail_error = self.ui.upgrade().is_some_and(|ui| {
            ui.set_initial_loading(false);
            ui.set_mutation_loading(false);
            ui.set_undo_loading(false);
            ui.set_mail_actions_enabled(false);
            ui.set_delete_dialog_open(false);
            ui.set_snackbar_can_undo(false);
            ui.set_snackbar_visible(false);
            ui.set_account_operation_loading(false);
            ui.set_sync_loading(false);
            ui.set_account_operation_stage(SharedString::default());
            ui.set_account_operation_error(
                "The account service stopped. Restart Nivalis and try again.".into(),
            );
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

    fn sync_mutation_ui(&self) {
        let (blocked, request_pending) = {
            let state = self.state.borrow();
            (
                state.mutation_blocks_actions(),
                state.mutation_request_pending(),
            )
        };
        if let Some(ui) = self.ui.upgrade() {
            let has_accounts = self
                .catalog
                .borrow()
                .as_ref()
                .is_some_and(|catalog| catalog.len() > 0);
            let mailbox_ready =
                has_accounts && !ui.get_mailbox_loading() && !ui.get_mailbox_error();
            ui.set_mutation_loading(blocked);
            ui.set_undo_loading(request_pending);
            ui.set_mail_actions_enabled(mailbox_ready);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SummaryState {
    unread: bool,
    starred: bool,
}

fn visible_summary_state(model: &VecModel<MailSummary>, key: EntityKey) -> Option<SummaryState> {
    (0..model.row_count()).find_map(|index| {
        let summary = model.row_data(index)?;
        (EntityKey::parse(summary.id.as_str()) == Some(key)).then_some(SummaryState {
            unread: summary.unread,
            starred: summary.starred,
        })
    })
}

fn desired_star_action(summary: SummaryState) -> MailAction {
    MailAction::SetStarred(!summary.starred)
}

fn desired_unread_action(summary: SummaryState) -> Option<MailAction> {
    (!summary.unread).then_some(MailAction::SetUnread(true))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MutationSuccessFeedback {
    status: &'static str,
    snackbar: &'static str,
    close_reader: bool,
    close_delete_dialog: bool,
}

fn sync_success_feedback(imported: u8, has_more: bool) -> String {
    match (imported, has_more) {
        (0, false) => "Inbox is up to date".to_owned(),
        (0, true) => "Checked one bounded range; sync again for more".to_owned(),
        (1, false) => "Imported 1 new message".to_owned(),
        (1, true) => "Imported 1 new message; sync again for more".to_owned(),
        (count, false) => format!("Imported {count} new messages"),
        (count, true) => format!("Imported {count} new messages; sync again for more"),
    }
}

fn mutation_success_feedback(intent: MutationIntent) -> MutationSuccessFeedback {
    match intent {
        MutationIntent::SetUnread { unread: true, .. } => MutationSuccessFeedback {
            status: "Message marked unread",
            snackbar: "Message marked unread",
            close_reader: false,
            close_delete_dialog: false,
        },
        MutationIntent::SetUnread { unread: false, .. } => MutationSuccessFeedback {
            status: "Message marked read",
            snackbar: "Message marked read",
            close_reader: false,
            close_delete_dialog: false,
        },
        MutationIntent::SetStarred { starred: true, .. } => MutationSuccessFeedback {
            status: "Star added",
            snackbar: "Star added",
            close_reader: false,
            close_delete_dialog: false,
        },
        MutationIntent::SetStarred { starred: false, .. } => MutationSuccessFeedback {
            status: "Star removed",
            snackbar: "Star removed",
            close_reader: false,
            close_delete_dialog: false,
        },
        MutationIntent::Archive { .. } => MutationSuccessFeedback {
            status: "Message archived",
            snackbar: "Message archived",
            close_reader: false,
            close_delete_dialog: false,
        },
        MutationIntent::MoveToTrash { .. } => MutationSuccessFeedback {
            status: "Message moved to Trash",
            snackbar: "Message moved to Trash",
            close_reader: true,
            close_delete_dialog: true,
        },
        MutationIntent::DeletePermanently { .. } => MutationSuccessFeedback {
            status: "Message permanently deleted",
            snackbar: "Message permanently deleted",
            close_reader: true,
            close_delete_dialog: true,
        },
        MutationIntent::UndoTrash { .. } => MutationSuccessFeedback {
            status: "Message restored",
            snackbar: "Message restored",
            close_reader: false,
            close_delete_dialog: false,
        },
    }
}

fn unix_time_ms(now: SystemTime) -> Option<i64> {
    let elapsed = now.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(elapsed.as_millis()).ok()
}

fn undo_remaining(expires_at_ms: i64, now: SystemTime) -> Option<Duration> {
    let deadline =
        UNIX_EPOCH.checked_add(Duration::from_millis(u64::try_from(expires_at_ms).ok()?))?;
    let remaining = deadline.duration_since(now).ok()?;
    (!remaining.is_zero()).then_some(remaining)
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

    const fn mailbox_changed() -> Self {
        Self {
            title: "Mailbox view changed",
            detail: "The requested page was replaced by a newer mailbox view. Continue from the current page.",
        }
    }

    const fn mutation_result() -> Self {
        Self {
            title: "Message change could not be verified",
            detail: "Nivalis will refresh the mailbox before another message change is allowed.",
        }
    }

    const fn system_clock() -> Self {
        Self {
            title: "Undo is unavailable",
            detail: "Nivalis could not read a valid system time. Check the clock and try again.",
        }
    }

    const fn account_port() -> Self {
        Self {
            title: "Check the IMAP port",
            detail: "Enter a port from 1 to 65535. Secure IMAP normally uses 993.",
        }
    }

    const fn account_password() -> Self {
        Self {
            title: "Enter an app password",
            detail: "Use a non-empty app password from your mail provider, then try again.",
        }
    }

    const fn account_session_limit() -> Self {
        Self {
            title: "Account session reached a safety limit",
            detail: "Restart Nivalis before changing another account.",
        }
    }

    const fn account_busy() -> Self {
        Self {
            title: "An account action is already running",
            detail: "Wait for the current account action to finish, or cancel the connection check.",
        }
    }

    const fn account_result() -> Self {
        Self {
            title: "Account result could not be verified",
            detail: "Refresh the account list and try the action again.",
        }
    }

    const fn account_runtime() -> Self {
        Self {
            title: "Account action could not start",
            detail: "The interface event loop is unavailable. Restart Nivalis and try again.",
        }
    }
}

fn parse_imap_port(value: &str) -> Option<u16> {
    if value.is_empty() || value.trim() != value || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    value.parse().ok().filter(|port| *port != 0)
}

fn account_accent(address: &str) -> u32 {
    const PALETTE: [u32; 6] = [0x315f4e, 0x3f5f78, 0x6a5741, 0x76546a, 0x516541, 0x73534a];
    let index = address
        .bytes()
        .fold(0_usize, |value, byte| value.wrapping_add(byte as usize))
        % PALETTE.len();
    PALETTE[index]
}

fn account_validation_error(error: AccountValidationError) -> UserError {
    match error {
        AccountValidationError::Name => UserError {
            title: "Check the account name",
            detail: "Enter a short name that identifies this account.",
        },
        AccountValidationError::Address => UserError {
            title: "Check the email address",
            detail: "Enter a complete email address, such as name@example.com.",
        },
        AccountValidationError::Login => UserError {
            title: "Check the login name",
            detail: "Enter the login name required by your mail provider. It is usually your email address.",
        },
        AccountValidationError::Host => UserError {
            title: "Check the IMAP server",
            detail: "Enter a valid server name, such as imap.example.com. Nivalis always verifies its certificate.",
        },
        AccountValidationError::Port => UserError::account_port(),
        AccountValidationError::CredentialKey
        | AccountValidationError::Accent
        | AccountValidationError::Generation
        | AccountValidationError::Timestamp => UserError::account_result(),
    }
}

fn account_submit_error(error: AccountOperationSubmitError) -> UserError {
    match error {
        AccountOperationSubmitError::Busy => UserError::account_busy(),
        AccountOperationSubmitError::Closed => UserError {
            title: "Account service stopped",
            detail: "Restart Nivalis before changing this account.",
        },
    }
}

fn account_response_error(_error: AccountOperationResponseError) -> UserError {
    UserError {
        title: "Account action stopped",
        detail: "The account service closed before the action finished. Restart Nivalis and try again.",
    }
}

fn account_operation_error(failure: AccountOperationFailure) -> UserError {
    match failure.kind {
        AccountWorkflowFailureKind::InboxSync(kind) => inbox_sync_error(kind),
        AccountWorkflowFailureKind::Diagnostic(kind) => account_diagnostic_error(kind),
        AccountWorkflowFailureKind::Credential(_) => UserError {
            title: "App password is unavailable",
            detail: "Unlock the system credential service, then run the account action again.",
        },
        AccountWorkflowFailureKind::CredentialReplyClosed => UserError {
            title: "Credential service stopped",
            detail: "Restart Nivalis, unlock the system credential service, and try again.",
        },
        AccountWorkflowFailureKind::Database(FailureKind::Conflict) => UserError {
            title: "Account changed",
            detail: "Reopen account status to use its latest settings, then try again.",
        },
        AccountWorkflowFailureKind::Database(FailureKind::NotFound) => UserError {
            title: "Account is no longer available",
            detail: "Refresh the account list and choose another account.",
        },
        AccountWorkflowFailureKind::Database(FailureKind::ResourceLimit) => UserError {
            title: "Account limit reached",
            detail: "Remove an unused account or reduce its local data before trying again.",
        },
        AccountWorkflowFailureKind::Database(FailureKind::Database)
        | AccountWorkflowFailureKind::Database(FailureKind::Migration) => UserError {
            title: "Local account data is unavailable",
            detail: "Check local storage permissions and restart Nivalis before trying again.",
        },
        AccountWorkflowFailureKind::Busy => UserError::account_busy(),
        AccountWorkflowFailureKind::InvalidLocator
        | AccountWorkflowFailureKind::UnexpectedReply => UserError::account_result(),
    }
}

fn inbox_sync_error(kind: InboxSyncFailureKind) -> UserError {
    match kind {
        InboxSyncFailureKind::Authentication => UserError {
            title: "Inbox sign-in was rejected",
            detail: "Check that the login and app password are current. Remove and add the account again to replace them, then sync Inbox.",
        },
        InboxSyncFailureKind::Permission => UserError {
            title: "Inbox access was denied",
            detail: "Enable IMAP access with your provider, then sync Inbox again.",
        },
        InboxSyncFailureKind::Certificate => UserError {
            title: "Server identity could not be verified",
            detail: "Check the IMAP server name and system trust settings, then sync again. Certificate verification cannot be bypassed.",
        },
        InboxSyncFailureKind::Timeout => UserError {
            title: "Inbox sync timed out",
            detail: "Check the network and server address, then sync Inbox again.",
        },
        InboxSyncFailureKind::Offline => UserError {
            title: "IMAP server is unreachable",
            detail: "Reconnect to the network or correct the server address, then sync Inbox again.",
        },
        InboxSyncFailureKind::Protocol => UserError {
            title: "Server response was not supported",
            detail: "Confirm that this is an IMAP-over-TLS server on the selected port, then sync again.",
        },
        InboxSyncFailureKind::ResourceLimit => UserError {
            title: "Inbox response exceeds a safety limit",
            detail: "Move the oversized message out of Inbox with your provider's web app, then sync again.",
        },
        InboxSyncFailureKind::Cancelled => UserError {
            title: "Inbox sync was cancelled",
            detail: "Run Sync inbox again when you are ready.",
        },
        InboxSyncFailureKind::UidValidityChanged => UserError {
            title: "Inbox identity changed",
            detail: "Open Accounts, remove and add this account again to rebuild its local Inbox cache.",
        },
        InboxSyncFailureKind::MalformedContent => UserError {
            title: "A message could not be imported",
            detail: "Move the malformed message out of Inbox with your provider's web app, then sync again.",
        },
        InboxSyncFailureKind::Storage => UserError {
            title: "Inbox could not be saved",
            detail: "Check available disk space and local storage permissions, then sync Inbox again.",
        },
    }
}

fn account_diagnostic_error(kind: AccountDiagnosticKind) -> UserError {
    match kind {
        AccountDiagnosticKind::Authentication => UserError {
            title: "Sign-in was rejected",
            detail: "Check that the login and app password are current. Remove and add the account again to replace them.",
        },
        AccountDiagnosticKind::Permission => UserError {
            title: "Inbox access was denied",
            detail: "Enable IMAP access with your provider, then check the connection again.",
        },
        AccountDiagnosticKind::Certificate => UserError {
            title: "Server identity could not be verified",
            detail: "Check the IMAP server name and system trust settings. Certificate verification cannot be bypassed.",
        },
        AccountDiagnosticKind::Timeout => UserError {
            title: "Connection check timed out",
            detail: "Check the network and server address, then run the connection check again.",
        },
        AccountDiagnosticKind::Offline => UserError {
            title: "IMAP server is unreachable",
            detail: "Reconnect to the network or correct the server address, then try again.",
        },
        AccountDiagnosticKind::Protocol => UserError {
            title: "Server response was not supported",
            detail: "Confirm that this is an IMAP-over-TLS server on the selected port, then try again.",
        },
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
        SessionError::MailboxRequestPending => UserError {
            title: "Mailbox navigation is already running",
            detail: "Wait for the current page to finish loading before trying again.",
        },
        SessionError::MutationRequestPending | SessionError::MutationRefreshPending => UserError {
            title: "Message change is already running",
            detail: "Wait for the mailbox to refresh before changing another message.",
        },
        SessionError::UndoUnavailable | SessionError::UndoExpired => UserError {
            title: "Undo is no longer available",
            detail: "The five-second undo period ended or a newer Trash action replaced it.",
        },
        SessionError::NavigationUnavailable => UserError {
            title: "That mailbox page is unavailable",
            detail: "The mailbox may have changed. Refresh it and try again.",
        },
        SessionError::EmptyNavigationPage => UserError {
            title: "Mailbox contents changed",
            detail: "Nivalis will refresh the first page so you can continue.",
        },
        SessionError::MailboxPageTooLarge => UserError::mail_data(),
        SessionError::InvalidIdentity
        | SessionError::InvalidFolder
        | SessionError::MessageNotVisible => UserError::selection(),
        SessionError::RequestIdExhausted
        | SessionError::GenerationExhausted
        | SessionError::PageNumberExhausted => UserError {
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

fn mutation_rejection(error: MutationSubmitError) -> UserError {
    match error {
        MutationSubmitError::Busy => submit_error(SubmitError::Busy),
        MutationSubmitError::Unavailable => submit_error(SubmitError::Closed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{core, store::sqlite};
    use rusqlite::Connection;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
        thread,
        time::Instant,
    };

    const CONTROLLER_TIMEOUT: Duration = Duration::from_secs(5);
    static NEXT_DATABASE_ID: AtomicU64 = AtomicU64::new(1);

    struct TestDatabase {
        directory: PathBuf,
        path: PathBuf,
    }

    impl TestDatabase {
        fn new(label: &str) -> Self {
            let unique = NEXT_DATABASE_ID.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "nivalis-controller-{label}-{}-{unique}",
                std::process::id()
            ));
            let path = directory.join("mail.sqlite3");
            let (client, replies, runtime, _info) =
                sqlite::spawn(path.clone()).expect("initialize controller test database");
            drop(client);
            drop(replies);
            runtime
                .shutdown()
                .expect("close initialized controller test database");
            Self { directory, path }
        }

        fn seed_bounded_mailbox(&self) {
            let connection = Connection::open(&self.path).expect("open controller fixture");
            connection
                .execute_batch(include_str!("../scripts/fixtures/memory.sql"))
                .expect("seed bounded controller fixture");
            connection
                .execute_batch(
                    "WITH RECURSIVE sequence(id) AS (
                         VALUES (1)
                         UNION ALL
                         SELECT id + 1 FROM sequence WHERE id < 64
                     )
                     INSERT INTO folders (id, account_id, remote_key, name, role)
                     SELECT 1000 + id, id, 'archive', 'Archive', 'archive' FROM sequence
                     UNION ALL
                     SELECT 2000 + id, id, 'trash', 'Trash', 'trash' FROM sequence;",
                )
                .expect("add mutation target folders");
        }

        fn seed_dirty_statistics(&self) {
            let connection = Connection::open(&self.path).expect("open dirty controller fixture");
            connection
                .execute_batch(
                    "PRAGMA foreign_keys = ON;
                     INSERT INTO accounts (
                         id, provider, remote_key, name, address, sort_order, state, accent_rgb
                     ) VALUES (
                         1, 'imap', 'dirty-account', 'Dirty account',
                         'dirty@example.test', 1, 'active', 0
                     );
                     INSERT INTO messages (
                         id, account_id, remote_key, sender_name, sender_address,
                         subject, preview, received_at_ms
                     ) VALUES (
                         1, 1, 'dirty-message', 'Sender', 'sender@example.test',
                         'Unreconciled message', 'Preview', 1700000000000
                     );",
                )
                .expect("seed dirty controller fixture");
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    struct ControllerHarness {
        ui: AppWindow,
        controller: Rc<Controller>,
        events: mpsc::Receiver<Event>,
        event_worker: Option<thread::JoinHandle<()>>,
        runtime: Option<core::CoreRuntime>,
    }

    impl ControllerHarness {
        fn start(path: &Path) -> Self {
            let (core, mut core_events, runtime) =
                core::spawn(path.to_path_buf()).expect("start production core");
            let (event_tx, events) = mpsc::channel();
            let event_worker = thread::spawn(move || {
                while let Some(event) = core_events.blocking_recv() {
                    if event_tx.send(event).is_err() {
                        break;
                    }
                }
            });
            let ui = AppWindow::new().expect("create test AppWindow");
            let controller = Rc::new(Controller::new(&ui, core));
            controller.install_handlers(&ui);
            controller.start();
            Self {
                ui,
                controller,
                events,
                event_worker: Some(event_worker),
                runtime: Some(runtime),
            }
        }

        fn drive_until(&mut self, label: &str, ready: impl Fn(&AppWindow) -> bool) {
            self.drive_until_controller(label, |ui, _controller| ready(ui));
        }

        fn drive_until_controller(
            &mut self,
            label: &str,
            ready: impl Fn(&AppWindow, &Controller) -> bool,
        ) {
            let deadline = Instant::now() + CONTROLLER_TIMEOUT;
            while !ready(&self.ui, &self.controller) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let event = self
                    .events
                    .recv_timeout(remaining)
                    .unwrap_or_else(|error| panic!("controller did not reach {label}: {error}"));
                self.controller.handle_event(event);
            }
        }

        fn shutdown(mut self) {
            self.runtime
                .take()
                .expect("controller runtime")
                .shutdown()
                .expect("stop controller runtime");
            self.event_worker
                .take()
                .expect("controller event worker")
                .join()
                .expect("join controller event worker");
        }
    }

    fn model_contains(ui: &AppWindow, id: &str) -> bool {
        let mails = ui.get_mails();
        (0..mails.row_count()).any(|index| {
            mails
                .row_data(index)
                .is_some_and(|summary| summary.id.as_str() == id)
        })
    }

    fn model_summary(ui: &AppWindow, id: &str) -> MailSummary {
        let mails = ui.get_mails();
        (0..mails.row_count())
            .find_map(|index| {
                mails
                    .row_data(index)
                    .filter(|summary| summary.id.as_str() == id)
            })
            .unwrap_or_else(|| panic!("mailbox does not contain message {id}"))
    }

    fn mailbox_ready(ui: &AppWindow, row_count: usize, page: i32) -> bool {
        !ui.get_initial_loading()
            && !ui.get_mailbox_loading()
            && !ui.get_mailbox_navigation_loading()
            && !ui.get_mutation_loading()
            && !ui.get_mailbox_error()
            && ui.get_mailbox_page_number() == page
            && ui.get_mails().row_count() == row_count
    }

    fn summary(id: i64, unread: bool, starred: bool) -> MailSummary {
        MailSummary {
            id: EntityKey::new(id).unwrap().encode(),
            account_id: "1".into(),
            account_label: "Account".into(),
            sender: "Sender".into(),
            initials: "S".into(),
            subject: "Subject".into(),
            preview: "Preview".into(),
            time: "Now".into(),
            unread,
            starred,
            has_attachment: false,
            avatar_color: slint::Color::from_rgb_u8(1, 2, 3),
        }
    }

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

    #[test]
    fn visible_summary_drives_desired_absolute_state() {
        let model = VecModel::from(vec![summary(1, false, false), summary(2, true, true)]);

        let first = visible_summary_state(&model, EntityKey::new(1).unwrap()).unwrap();
        let second = visible_summary_state(&model, EntityKey::new(2).unwrap()).unwrap();
        assert_eq!(desired_star_action(first), MailAction::SetStarred(true));
        assert_eq!(desired_star_action(second), MailAction::SetStarred(false));
        assert_eq!(
            desired_unread_action(first),
            Some(MailAction::SetUnread(true))
        );
        assert_eq!(desired_unread_action(second), None);
        assert_eq!(
            visible_summary_state(&model, EntityKey::new(3).unwrap()),
            None
        );
    }

    #[test]
    fn undo_timer_uses_the_absolute_receipt_deadline() {
        let now = UNIX_EPOCH + Duration::from_millis(10_250);
        assert_eq!(
            undo_remaining(12_000, now),
            Some(Duration::from_millis(1_750))
        );
        assert_eq!(undo_remaining(10_250, now), None);
        assert_eq!(undo_remaining(10_249, now), None);

        let fractional_now = now + Duration::from_micros(500);
        assert_eq!(
            undo_remaining(12_000, fractional_now),
            Some(Duration::from_micros(1_749_500))
        );
    }

    #[test]
    fn success_feedback_is_stable_and_only_removals_close_the_reader() {
        let id = crate::store::sqlite::MessageId::new(1).unwrap();
        let cases = [
            (
                MutationIntent::SetUnread { id, unread: true },
                "Message marked unread",
                false,
                false,
            ),
            (
                MutationIntent::SetStarred { id, starred: false },
                "Star removed",
                false,
                false,
            ),
            (
                MutationIntent::Archive { id },
                "Message archived",
                false,
                false,
            ),
            (
                MutationIntent::MoveToTrash { id },
                "Message moved to Trash",
                true,
                true,
            ),
            (
                MutationIntent::DeletePermanently { id },
                "Message permanently deleted",
                true,
                true,
            ),
            (
                MutationIntent::UndoTrash {
                    id,
                    token: crate::store::sqlite::undo_token_for_test(7),
                },
                "Message restored",
                false,
                false,
            ),
        ];

        for (intent, message, close_reader, close_delete_dialog) in cases {
            let feedback = mutation_success_feedback(intent);
            assert_eq!(feedback.status, message);
            assert_eq!(feedback.snackbar, message);
            assert_eq!(feedback.close_reader, close_reader);
            assert_eq!(feedback.close_delete_dialog, close_delete_dialog);
        }
    }

    #[test]
    fn inbox_sync_feedback_distinguishes_current_and_imported_mail() {
        assert_eq!(sync_success_feedback(0, false), "Inbox is up to date");
        assert_eq!(
            sync_success_feedback(0, true),
            "Checked one bounded range; sync again for more"
        );
        assert_eq!(sync_success_feedback(1, false), "Imported 1 new message");
        assert_eq!(
            sync_success_feedback(16, true),
            "Imported 16 new messages; sync again for more"
        );
    }

    #[test]
    fn mutation_errors_do_not_disclose_database_messages() {
        let failure = DbFailure {
            kind: FailureKind::Conflict,
            message: "secret sqlite statement".into(),
        };
        for error in [
            database_error(&failure),
            UserError::mutation_result(),
            mutation_rejection(MutationSubmitError::Busy),
            mutation_rejection(MutationSubmitError::Unavailable),
        ] {
            assert!(!error.title.is_empty());
            assert!(!error.detail.is_empty());
            assert!(!error.title.contains("secret"));
            assert!(!error.detail.contains("secret"));
            assert!(!error.detail.contains("sqlite"));
        }
    }

    #[test]
    fn account_inputs_and_diagnostic_guidance_are_bounded_and_stable() {
        assert_eq!(parse_imap_port("993"), Some(993));
        for invalid in ["", "0", " 993", "993 ", "-1", "65536", "imap"] {
            assert_eq!(parse_imap_port(invalid), None, "accepted {invalid:?}");
        }
        assert_eq!(
            account_accent("user@example.test"),
            account_accent("user@example.test")
        );
        assert!(account_accent("user@example.test") <= 0x00ff_ffff);

        let cases = [
            (
                AccountDiagnosticKind::Authentication,
                "Sign-in was rejected",
                "app password",
            ),
            (
                AccountDiagnosticKind::Permission,
                "Inbox access was denied",
                "Enable IMAP",
            ),
            (
                AccountDiagnosticKind::Certificate,
                "Server identity could not be verified",
                "cannot be bypassed",
            ),
            (
                AccountDiagnosticKind::Timeout,
                "Connection check timed out",
                "network",
            ),
            (
                AccountDiagnosticKind::Offline,
                "IMAP server is unreachable",
                "Reconnect",
            ),
            (
                AccountDiagnosticKind::Protocol,
                "Server response was not supported",
                "IMAP-over-TLS",
            ),
        ];
        for (kind, title, next_step) in cases {
            let error = account_diagnostic_error(kind);
            assert_eq!(error.title, title);
            assert!(error.detail.contains(next_step));
            assert!(!error.detail.contains("secret server response"));
        }
    }

    #[test]
    fn inbox_sync_failures_explain_the_next_action() {
        let cases = [
            (InboxSyncFailureKind::Authentication, "app password"),
            (InboxSyncFailureKind::Permission, "Enable IMAP"),
            (InboxSyncFailureKind::Certificate, "trust settings"),
            (InboxSyncFailureKind::Timeout, "sync Inbox again"),
            (InboxSyncFailureKind::Offline, "Reconnect"),
            (InboxSyncFailureKind::Protocol, "IMAP-over-TLS"),
            (InboxSyncFailureKind::ResourceLimit, "provider's web app"),
            (InboxSyncFailureKind::Cancelled, "Sync inbox again"),
            (InboxSyncFailureKind::UidValidityChanged, "Open Accounts"),
            (InboxSyncFailureKind::MalformedContent, "provider's web app"),
            (InboxSyncFailureKind::Storage, "disk space"),
        ];

        for (kind, next_step) in cases {
            let error = inbox_sync_error(kind);
            assert!(!error.title.is_empty());
            assert!(
                error.detail.contains(next_step),
                "{kind:?}: {}",
                error.detail
            );
        }
    }

    #[test]
    fn production_controller_drives_sqlite_success_empty_and_error_states() {
        i_slint_backend_testing::init_no_event_loop();

        let database = TestDatabase::new("mailbox");
        database.seed_bounded_mailbox();
        let mut harness = ControllerHarness::start(&database.path);
        harness.drive_until("initial bounded mailbox", |ui| mailbox_ready(ui, 50, 1));

        assert!(harness.ui.get_has_accounts());
        assert!(harness.ui.get_mail_actions_enabled());
        assert_eq!(harness.ui.get_accounts().row_count(), 65);

        harness.ui.invoke_manage_account("1".into());
        assert!(harness.ui.get_account_status_open());
        assert_eq!(harness.ui.get_managed_account_id().as_str(), "1");
        assert_eq!(
            harness.ui.get_managed_account_address().as_str(),
            "account-1@example.test"
        );
        harness.ui.set_account_status_open(false);

        harness.ui.invoke_add_account(
            "Personal".into(),
            "user@example.test".into(),
            "user@example.test".into(),
            "imap.example.test".into(),
            "0".into(),
            "not-retained".into(),
        );
        assert_eq!(
            harness.ui.get_account_operation_error().as_str(),
            "Enter a port from 1 to 65535. Secure IMAP normally uses 993."
        );
        assert!(!harness.ui.get_account_operation_loading());

        assert_eq!(harness.ui.get_message_total(), 51);
        assert!(harness.ui.get_has_next_mailbox_page());
        assert!(model_contains(&harness.ui, "51"));

        harness.ui.invoke_next_mailbox_page();
        harness.drive_until("second mailbox page", |ui| mailbox_ready(ui, 1, 2));
        assert!(model_contains(&harness.ui, "1"));
        assert!(harness.ui.get_has_previous_mailbox_page());

        harness.ui.invoke_previous_mailbox_page();
        harness.drive_until("first mailbox page", |ui| mailbox_ready(ui, 50, 1));
        assert!(model_contains(&harness.ui, "51"));

        harness.ui.invoke_switch_account("1".into());
        harness.drive_until("single-account mailbox", |ui| mailbox_ready(ui, 1, 1));
        assert_eq!(harness.ui.get_active_account_id().as_str(), "1");
        assert!(model_contains(&harness.ui, "1"));

        harness.ui.invoke_switch_account("".into());
        harness.drive_until("all-account mailbox", |ui| mailbox_ready(ui, 50, 1));
        assert!(harness.ui.get_active_account_id().is_empty());

        harness.ui.set_search_query("message 49".into());
        harness.ui.invoke_query_mail("message 49".into());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
        harness.drive_until("FTS mailbox result", |ui| mailbox_ready(ui, 1, 1));
        assert!(model_contains(&harness.ui, "49"));

        harness.ui.set_search_query("".into());
        harness.ui.invoke_query_mail("".into());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
        harness.drive_until("cleared FTS mailbox", |ui| mailbox_ready(ui, 50, 1));

        harness.ui.set_detail_open(true);
        harness.ui.invoke_select_mail("51".into());
        harness.drive_until("selected message detail", |ui| {
            !ui.get_detail_loading() && ui.get_selected_mail().id.as_str() == "51"
        });
        assert_eq!(harness.ui.get_selected_mail().body.as_str().len(), 65_536);

        assert!(model_summary(&harness.ui, "51").starred);
        harness.ui.invoke_toggle_star("51".into());
        harness.drive_until("star mutation refresh", |ui| {
            mailbox_ready(ui, 50, 1) && !model_summary(ui, "51").starred
        });

        assert!(!model_summary(&harness.ui, "50").unread);
        harness.ui.invoke_mark_unread("50".into());
        harness.drive_until("unread mutation refresh", |ui| {
            mailbox_ready(ui, 50, 1) && model_summary(ui, "50").unread
        });

        harness.ui.invoke_archive("51".into());
        harness.drive_until("archive mutation refresh", |ui| {
            mailbox_ready(ui, 50, 1) && !model_contains(ui, "51")
        });

        harness.ui.invoke_delete_mail("49".into());
        harness.drive_until("Trash mutation refresh", |ui| {
            mailbox_ready(ui, 49, 1) && !model_contains(ui, "49") && ui.get_snackbar_can_undo()
        });
        harness.ui.invoke_undo_delete();
        harness.drive_until("Trash undo refresh", |ui| {
            mailbox_ready(ui, 50, 1) && model_contains(ui, "49") && !ui.get_snackbar_can_undo()
        });

        harness.ui.invoke_delete_mail("48".into());
        harness.drive_until("permanent-delete setup", |ui| {
            mailbox_ready(ui, 49, 1) && !model_contains(ui, "48")
        });
        harness.ui.invoke_filter_folder("Trash".into());
        harness.drive_until("Trash folder", |ui| mailbox_ready(ui, 1, 1));
        assert!(model_contains(&harness.ui, "48"));
        harness.ui.invoke_delete_mail("48".into());
        harness.drive_until("permanent deletion", |ui| mailbox_ready(ui, 0, 1));

        harness.ui.invoke_filter_folder("Archive".into());
        harness.drive_until("Archive folder", |ui| mailbox_ready(ui, 1, 1));
        assert!(model_contains(&harness.ui, "51"));
        harness.shutdown();

        let connection = Connection::open(&database.path).expect("inspect controller database");
        let persisted = connection
            .query_row(
                "SELECT
                     (SELECT starred FROM messages WHERE id = 51),
                     (SELECT unread FROM messages WHERE id = 50),
                     (SELECT count(*) FROM messages WHERE id = 48),
                     (SELECT count(*)
                        FROM message_tombstones
                       WHERE account_id = 48 AND remote_key = 'message-48'),
                     (SELECT f.role
                        FROM message_folders AS mf
                        JOIN folders AS f ON f.id = mf.folder_id
                       WHERE mf.message_id = 49)",
                [],
                |row| {
                    Ok((
                        row.get::<_, bool>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .expect("read persisted controller outcomes");
        assert_eq!(persisted, (false, true, 0, 1, "inbox".to_owned()));
        drop(connection);

        let empty_database = TestDatabase::new("empty");
        let mut empty = ControllerHarness::start(&empty_database.path);
        empty.drive_until("empty mailbox", |ui| mailbox_ready(ui, 0, 1));
        assert!(!empty.ui.get_has_accounts());
        assert!(!empty.ui.get_mail_actions_enabled());
        assert_eq!(empty.ui.get_status_text().as_str(), "Local cache is empty");
        empty.shutdown();

        let dirty_database = TestDatabase::new("dirty");
        dirty_database.seed_dirty_statistics();
        let mut dirty = ControllerHarness::start(&dirty_database.path);
        dirty.drive_until_controller("dirty-statistics error", |ui, controller| {
            ui.get_mailbox_error()
                && !ui.get_initial_loading()
                && !ui.get_mailbox_loading()
                && !controller.state.borrow().accounts_pending()
                && !controller.state.borrow().mailbox_pending()
        });
        assert!(!dirty.ui.get_has_accounts());
        assert!(!dirty.ui.get_mail_actions_enabled());
        assert_eq!(
            dirty.ui.get_mailbox_error_title().as_str(),
            "Local mail needs attention"
        );
        assert_eq!(
            dirty.ui.get_mailbox_error_detail().as_str(),
            "Stored mailbox state is inconsistent. Retry after the local cache has been repaired."
        );

        let connection = Connection::open(&dirty_database.path).expect("open repair connection");
        sqlite::rebuild_account_stats_for_test(&connection, 1)
            .expect("repair dirty mailbox statistics");
        drop(connection);
        dirty.ui.invoke_retry_mailbox();
        dirty.drive_until("repaired mailbox retry", |ui| mailbox_ready(ui, 0, 1));
        assert!(dirty.ui.get_has_accounts());
        assert!(dirty.ui.get_mail_actions_enabled());
        dirty.shutdown();
    }
}
