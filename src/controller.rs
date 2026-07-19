mod session;

use self::session::{
    DetailAcceptance, MailAction, MailboxAcceptance, MailboxIntent, MutationCompletion,
    MutationIntent, MutationScope, ReadSession, SessionError,
};
use crate::core::{
    AccountDirectoryLoadError, CoreHandle, Event, EventReceiver, MailboxLoadError,
    MessageLoadError, MutationSubmitError, SubmitError,
};
use crate::presentation::sqlite::{
    AccountCatalog, ProjectedMailbox, ProjectedMailboxStats, ProjectionError,
};
use crate::presentation::{show_snackbar, show_snackbar_for};
use crate::store::sqlite::{
    AccountDirectory, DbFailure, FailureKind, MailboxPage, MessageDetail, Tagged,
};
use crate::ui_identity::{AccountKey, EntityKey};
use crate::{AccountItem, AppWindow, MailDetail, MailSummary};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::{
    cell::RefCell,
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
        self.snackbar_timer.stop();
        let show_detail_error = self.ui.upgrade().is_some_and(|ui| {
            ui.set_initial_loading(false);
            ui.set_mutation_loading(false);
            ui.set_undo_loading(false);
            ui.set_mail_actions_enabled(false);
            ui.set_delete_dialog_open(false);
            ui.set_snackbar_can_undo(false);
            ui.set_snackbar_visible(false);
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
            let mailbox_ready = self.catalog.borrow().is_some()
                && !ui.get_mailbox_loading()
                && !ui.get_mailbox_error();
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
}
