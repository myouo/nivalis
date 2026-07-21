mod session;

use self::session::{
    DetailAcceptance, MailAction, MailboxAcceptance, MailboxCommitEffect, MailboxIntent,
    MutationCompletion, MutationIntent, MutationScope, ReadSession, SessionError,
};
use crate::core::{
    AccountConfigDraft, AccountDirectoryLoadError, AccountOperation, AccountOperationFailure,
    AccountOperationResponseError, AccountOperationSubmitError, AccountOperationSuccess,
    AccountSetupMode, AccountSyncStatus, AccountWorkflowFailureKind, COMPOSE_BODY_BYTE_LIMIT,
    COMPOSE_SUBJECT_BYTE_LIMIT, COMPOSE_TO_FIELD_BYTE_LIMIT, ComposeDraftIdentity,
    ComposeDraftInput, ComposeFailure, ComposeFailureKind, ComposeOperation,
    ComposeOperationResponseError, ComposeOperationSubmitError, ComposeSuccess, CoreHandle, Event,
    EventReceiver, InboxSyncFailureKind, MailboxLoadError, MessageId, MessageLoadError,
    MutationSubmitError, OutboxCancelOutcome, OutboxDriverFault, OutboxErrorClass, OutboxState,
    OutboxStatus, OutboxSummary, OutboxSummaryPage, RequestId, SubmitError, UncertainResolution,
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
use crate::{AccountItem, AppWindow, MailDetail, MailSummary, OutboxItem};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const SYNC_REFRESH_COALESCE: Duration = Duration::from_millis(50);

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
    outbox_model: Rc<VecModel<OutboxItem>>,
    outbox_snapshots: RefCell<Box<[OutboxSummary]>>,
    account_task: RefCell<Option<slint::JoinHandle<()>>>,
    history_task: RefCell<Option<slint::JoinHandle<()>>>,
    content_task: RefCell<Option<slint::JoinHandle<()>>>,
    compose_task: RefCell<Option<slint::JoinHandle<()>>>,
    outbox_task: RefCell<Option<slint::JoinHandle<()>>>,
    outbox_busy: Cell<bool>,
    outbox_refresh_pending: Cell<bool>,
    outbox_cancel_pending: Cell<bool>,
    compose_identity: Cell<Option<ComposeDraftIdentity>>,
    compose_target: Cell<Option<AccountOperationTarget>>,
    compose_dirty: Cell<bool>,
    window_close_pending: Cell<bool>,
    account_request_id: Cell<u64>,
    account_cancelable: Cell<bool>,
    pending_account_selection: Cell<Option<i64>>,
    search_timer: Rc<Timer>,
    sync_refresh_timer: Rc<Timer>,
    pending_sync_refresh: Cell<Option<bool>>,
    remote_history_more: Cell<bool>,
    remote_append_pending: Cell<bool>,
    snackbar_timer: Rc<Timer>,
}

struct PendingMailboxProjection {
    acceptance: MailboxAcceptance,
    page: MailboxPage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutboxUiAction {
    Retry,
    ReleaseFailed,
    AssumeDelivered,
    ReleaseUncertain,
}

impl Controller {
    fn new(ui: &AppWindow, core: CoreHandle) -> Self {
        let mail_model = Rc::new(VecModel::from(Vec::<MailSummary>::new()));
        let account_model = Rc::new(VecModel::from(Vec::<AccountItem>::new()));
        let outbox_model = Rc::new(VecModel::from(Vec::<OutboxItem>::new()));
        ui.set_mails(ModelRc::from(mail_model.clone()));
        ui.set_accounts(ModelRc::from(account_model.clone()));
        ui.set_outbox_items(ModelRc::from(outbox_model.clone()));
        ui.set_has_accounts(false);
        ui.set_mail_actions_enabled(false);
        ui.set_mutation_loading(false);
        ui.set_undo_loading(false);
        ui.set_initial_loading(true);
        ui.set_mailbox_loading(true);
        ui.set_mailbox_has_more(false);
        ui.set_mailbox_loading_more(false);
        ui.set_mailbox_error(false);
        ui.set_detail_loading(false);
        ui.set_detail_error(false);
        ui.set_detail_content_pending(false);
        ui.set_detail_content_error(SharedString::default());
        ui.set_total_known(true);
        ui.set_data_source_label("SQLite cache".into());
        ui.set_status_text("Loading local cache".into());
        ui.set_account_operation_loading(false);
        ui.set_sync_loading(false);
        ui.set_account_operation_stage(SharedString::default());
        ui.set_account_operation_error(SharedString::default());
        ui.set_compose_enabled(false);
        ui.set_composer_loading(false);
        ui.set_composer_status(SharedString::default());
        ui.set_composer_error(SharedString::default());
        ui.set_outbox_open(false);
        ui.set_outbox_loading(false);
        ui.set_outbox_error(SharedString::default());
        ui.set_outbox_has_more(false);
        ui.set_outbox_count(0);
        ui.set_outbox_action_loading(false);
        ui.set_outbox_action_message_id(SharedString::default());
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
            outbox_model,
            outbox_snapshots: RefCell::new(Box::new([])),
            account_task: RefCell::new(None),
            history_task: RefCell::new(None),
            content_task: RefCell::new(None),
            compose_task: RefCell::new(None),
            outbox_task: RefCell::new(None),
            outbox_busy: Cell::new(false),
            outbox_refresh_pending: Cell::new(false),
            outbox_cancel_pending: Cell::new(false),
            compose_identity: Cell::new(None),
            compose_target: Cell::new(None),
            compose_dirty: Cell::new(false),
            window_close_pending: Cell::new(false),
            account_request_id: Cell::new(1),
            account_cancelable: Cell::new(false),
            pending_account_selection: Cell::new(None),
            search_timer: Rc::new(Timer::default()),
            sync_refresh_timer: Rc::new(Timer::default()),
            pending_sync_refresh: Cell::new(None),
            remote_history_more: Cell::new(false),
            remote_append_pending: Cell::new(false),
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
        ui.on_add_account(
            move |name, address, login, imap_host, imap_port, smtp_host, smtp_port, password| {
                controller.add_account(
                    name, address, login, imap_host, imap_port, smtp_host, smtp_port, password,
                );
            },
        );

        let controller = self.clone();
        ui.on_diagnose_account(move |key| controller.diagnose_account(key.as_str()));

        let controller = self.clone();
        ui.on_update_account_credential(move |key, password| {
            controller.update_account_credential(key.as_str(), password);
        });

        let controller = self.clone();
        ui.on_sync_account(move |key| controller.sync_account(key.as_str()));

        let controller = self.clone();
        ui.on_remove_account(move |key| controller.remove_account(key.as_str()));

        let controller = self.clone();
        ui.on_cancel_account_operation(move || controller.cancel_account_operation());

        let controller = self.clone();
        ui.on_retry_mailbox(move || controller.retry_mailbox());

        let controller = self.clone();
        ui.on_load_more_mail(move || controller.load_more_mail());

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

        let controller = self.clone();
        ui.on_open_composer(move || controller.open_composer());

        let controller = self.clone();
        ui.on_save_draft(move |to, subject, body| {
            controller.save_compose_draft(to, subject, body);
        });

        let controller = self.clone();
        ui.on_send_message(move |to, subject, body| {
            controller.queue_composed_message(to, subject, body);
        });

        let controller = self.clone();
        ui.on_compose_input_edited(move |field, value| {
            controller.bound_compose_input(field.as_str(), value);
        });

        let controller = self.clone();
        ui.on_window_close(move || controller.request_window_close());

        let controller = self.clone();
        ui.on_open_outbox(move || controller.load_outbox(true));

        let controller = self.clone();
        ui.on_reload_outbox(move || controller.load_outbox(true));

        let controller = self.clone();
        ui.on_close_outbox(move || controller.close_outbox());

        let controller = self.clone();
        ui.on_retry_outbox(move |key| {
            controller.change_outbox(key.as_str(), OutboxUiAction::Retry);
        });

        let controller = self.clone();
        ui.on_release_failed_outbox(move |key| {
            controller.change_outbox(key.as_str(), OutboxUiAction::ReleaseFailed);
        });

        let controller = self.clone();
        ui.on_assume_delivered_outbox(move |key| {
            controller.change_outbox(key.as_str(), OutboxUiAction::AssumeDelivered);
        });

        let controller = self.clone();
        ui.on_release_uncertain_outbox(move |key| {
            controller.change_outbox(key.as_str(), OutboxUiAction::ReleaseUncertain);
        });

        let controller = self.clone();
        ui.on_cancel_active_outbox(move |key| controller.cancel_outbox_attempt(key.as_str()));

        let controller = self.clone();
        ui.on_repair_outbox_credential(move |key| {
            controller.open_outbox_credential_repair(key.as_str());
        });
    }

    fn start(self: &Rc<Self>) {
        self.reload_mailbox();
        self.issue_accounts();
        self.load_outbox(false);
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
            ui.set_mailbox_has_more(false);
            ui.set_mailbox_loading_more(false);
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

    fn handle_event(self: &Rc<Self>, event: Event) {
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
            Event::AccountSyncStatus(status) => self.handle_account_sync_status(status),
            Event::OutboxStatus(status) => self.handle_outbox_status(status),
        }
    }

    fn handle_account_sync_status(self: &Rc<Self>, status: AccountSyncStatus) {
        match status {
            AccountSyncStatus::Synced {
                account_id,
                imported,
                has_more,
                historical,
                ..
            } => {
                self.issue_accounts();
                if !self.sync_status_is_visible(account_id) {
                    return;
                }
                if self.selected_account_matches(account_id)
                    && self.state.borrow().can_load_remote_history()
                {
                    self.remote_history_more.set(has_more);
                }
                if let Some(ui) = self.ui.upgrade()
                    && !foreground_feedback_active(&ui)
                {
                    ui.set_status_text(
                        background_sync_feedback(imported, has_more, historical).into(),
                    );
                }
                let refresh_visible_mailbox = self.ui.upgrade().is_some_and(|ui| {
                    should_refresh_synced_mailbox(
                        imported,
                        self.state.borrow().shows_inbox_sync_updates(),
                        ui.get_mailbox_loading(),
                        ui.get_mailbox_loading_more(),
                    )
                });
                if refresh_visible_mailbox {
                    self.schedule_sync_refresh(historical);
                }
            }
            AccountSyncStatus::Failed(failure) => {
                if failure
                    .account_id
                    .is_some_and(|account_id| !self.sync_status_is_visible(account_id))
                {
                    return;
                }
                let error = account_operation_error(failure);
                if let Some(ui) = self.ui.upgrade()
                    && !foreground_feedback_active(&ui)
                {
                    ui.set_status_text(error.title.into());
                    show_snackbar(&ui, error.detail, false, &self.snackbar_timer);
                }
            }
        }
    }

    fn sync_status_is_visible(&self, account_id: crate::store::sqlite::AccountId) -> bool {
        match self.state.borrow().account() {
            AccountKey::All => true,
            AccountKey::Account(selected) => selected.get() == account_id.get(),
        }
    }

    fn selected_account_matches(&self, account_id: crate::store::sqlite::AccountId) -> bool {
        matches!(
            self.state.borrow().account(),
            AccountKey::Account(selected) if selected.get() == account_id.get()
        )
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
        self.refresh_outbox_projection();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_has_accounts(has_accounts);
            ui.set_initial_loading(false);
        }
        self.refresh_active_account();
        self.refresh_managed_account();

        let pending_page = self.pending_mailbox.borrow_mut().take();
        if scope_reset {
            self.reset_remote_history_scope();
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
        let intent = acceptance.intent();
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
            next_cursor,
        } = projected;
        let row_count = rows.len();
        let (effect, replacement_pending) = {
            let mut state = self.state.borrow_mut();
            let effect = state.commit_mailbox(commit);
            (effect, state.mailbox_pending())
        };
        let Some(effect) = effect else {
            if !replacement_pending {
                self.fail_mailbox_request(acceptance.intent(), UserError::mailbox_changed());
            }
            return;
        };
        let state = self.state.borrow();
        if effect != MailboxCommitEffect::Preserve || self.mail_model.row_count() <= 50 {
            debug_assert_eq!(next_cursor, state.next_cursor());
        }
        let has_more = state.next_cursor().is_some()
            || (state.can_load_remote_history() && self.remote_history_more.get());
        drop(state);

        let selected_to_restore = (effect == MailboxCommitEffect::Replace)
            .then(|| self.selected_reader_key())
            .flatten();
        let selected_to_refresh = (effect == MailboxCommitEffect::Preserve)
            .then(|| self.selected_reader_key())
            .flatten();
        match effect {
            MailboxCommitEffect::Replace => {
                self.clear_reader();
                self.mail_model.set_vec(rows);
            }
            MailboxCommitEffect::Append => {
                for row in rows {
                    self.mail_model.push(row);
                }
            }
            MailboxCommitEffect::Extend { from } => {
                for row in rows.into_iter().skip(from) {
                    self.mail_model.push(row);
                }
            }
            MailboxCommitEffect::Preserve => {
                for (index, row) in rows.into_iter().enumerate() {
                    if index >= self.mail_model.row_count() {
                        break;
                    }
                    self.mail_model.set_row_data(index, row);
                }
            }
        }
        let loaded_count = self.mail_model.row_count();
        self.apply_stats(stats);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_loading(false);
            ui.set_mailbox_has_more(has_more);
            ui.set_mailbox_loading_more(false);
            ui.set_mailbox_error(false);
            ui.set_initial_loading(false);
            if intent != MailboxIntent::Refresh {
                ui.set_status_text(
                    if intent == MailboxIntent::Append {
                        format!("{loaded_count} cached messages loaded")
                    } else if has_more {
                        "More cached messages available".to_owned()
                    } else if row_count == 0 {
                        "Local cache is empty".to_owned()
                    } else {
                        "Local cache ready".to_owned()
                    }
                    .into(),
                );
            }
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
        if let Some(key) = selected_to_refresh
            && self.ui.upgrade().is_some_and(|ui| ui.get_detail_open())
            && visible_summary_state(self.mail_model.as_ref(), key).is_some()
        {
            self.select_message(key.encode());
        }
        if self.remote_append_pending.replace(false) {
            if self.state.borrow_mut().enable_remote_tail_navigation() {
                self.submit_mailbox_navigation(MailboxIntent::Append);
            } else {
                self.finish_history_load_without_refresh();
            }
        }
    }

    fn load_more_mail(self: &Rc<Self>) {
        if self.state.borrow().next_cursor().is_some() {
            self.submit_mailbox_navigation(MailboxIntent::Append);
        } else if self.state.borrow().can_load_remote_history() && self.remote_history_more.get() {
            self.sync_next_history_page();
        }
    }

    fn sync_next_history_page(self: &Rc<Self>) {
        let account = self.state.borrow().account();
        if !self.state.borrow().can_load_remote_history() {
            return;
        }
        let Some(target) = self.account_operation_target(account.encode().as_str()) else {
            self.fail_history_load(UserError::selection());
            return;
        };
        let Some(request_id) = self.next_account_request_id() else {
            self.fail_history_load(UserError::account_session_limit());
            return;
        };
        {
            let mut task = self.history_task.borrow_mut();
            if task.as_ref().is_some_and(|task| !task.is_finished()) {
                return;
            }
            task.take();
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_loading_more(true);
            ui.set_mailbox_error(false);
            ui.set_status_text("Loading 50 message headers in the background".into());
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
                self.fail_history_load(account_submit_error(failure.reason()));
                return;
            }
        };
        let weak = Rc::downgrade(self);
        let task = slint::spawn_local(async move {
            let result = response.await;
            let Some(controller) = weak.upgrade() else {
                return;
            };
            controller.history_task.borrow_mut().take();
            match result {
                Ok(reply) if reply.request_id == request_id => match reply.result {
                    Ok(AccountOperationSuccess::Synced {
                        account_id,
                        generation,
                        imported,
                        has_more,
                        historical,
                    }) if account_id == target.account_id
                        && generation == target.expected_generation
                        && controller.selected_account_matches(account_id) =>
                    {
                        controller.remote_history_more.set(has_more);
                        controller.issue_accounts();
                        if historical {
                            controller.remote_append_pending.set(true);
                            controller.refresh_after_historical_sync();
                        } else if imported > 0 {
                            controller.refresh_after_mutation();
                        } else {
                            controller.finish_history_load_without_refresh();
                        }
                    }
                    Err(failure) => {
                        controller.fail_history_load(account_operation_error(failure));
                    }
                    Ok(_) => controller.fail_history_load(UserError::account_result()),
                },
                Ok(_) => controller.fail_history_load(UserError::account_result()),
                Err(error) => {
                    controller.fail_history_load(account_response_error(error));
                }
            }
        });
        match task {
            Ok(task) => *self.history_task.borrow_mut() = Some(task),
            Err(_) => self.fail_history_load(UserError::account_runtime()),
        }
    }

    fn finish_history_load_without_refresh(&self) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_loading_more(false);
            ui.set_mailbox_has_more(
                self.state.borrow().next_cursor().is_some()
                    || (self.state.borrow().can_load_remote_history()
                        && self.remote_history_more.get()),
            );
            ui.set_status_text("All available messages loaded".into());
        }
    }

    fn fail_history_load(&self, error: UserError) {
        self.history_task.borrow_mut().take();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_loading_more(false);
            ui.set_status_text(error.title.into());
            show_snackbar(&ui, error.detail, false, &self.snackbar_timer);
        }
    }

    fn submit_mailbox_navigation(&self, intent: MailboxIntent) {
        let query = {
            let mut state = self.state.borrow_mut();
            match intent {
                MailboxIntent::First => state.issue_first_mailbox(),
                MailboxIntent::Append => state.issue_more_mailbox(),
                MailboxIntent::Refresh => state.issue_mailbox_refresh(),
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
            ui.set_mailbox_loading_more(intent == MailboxIntent::Append);
            ui.set_mailbox_error(false);
            if intent != MailboxIntent::Refresh {
                ui.set_status_text(
                    match intent {
                        MailboxIntent::First => "Refreshing local mail",
                        MailboxIntent::Append => "Loading more cached messages",
                        MailboxIntent::Refresh => unreachable!(),
                    }
                    .into(),
                );
            }
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
                .is_some_and(|ui| ui.get_mailbox_loading_more());
        if preserves_page {
            self.fail_navigation(error);
        } else {
            self.fail_mailbox(error);
        }
    }

    fn fail_navigation(&self, error: UserError) {
        self.remote_append_pending.set(false);
        if self.state.borrow().mutation_refresh_pending() {
            self.fail_mailbox(error);
            self.sync_mutation_ui();
            return;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_loading_more(false);
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

    fn handle_detail(self: &Rc<Self>, reply: Tagged<Option<MessageDetail>>) {
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
                let message_id = detail.id;
                let account_id = detail.account_id;
                let content_available = detail.content_available;
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
                    ui.set_detail_content_pending(!content_available);
                    ui.set_detail_content_error(SharedString::default());
                    ui.set_status_text(
                        if content_available {
                            "Message loaded from local cache"
                        } else {
                            "Headers loaded from local cache; fetching body in background"
                        }
                        .into(),
                    );
                }
                if content_available {
                    if let Some(task) = self.content_task.borrow_mut().take() {
                        task.abort();
                    }
                } else {
                    self.fetch_message_content(message_id, account_id);
                }
            }
        }
    }

    fn select_message(&self, key: SharedString) {
        if let Some(task) = self.content_task.borrow_mut().take() {
            task.abort();
        }
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
            ui.set_detail_content_pending(false);
            ui.set_detail_content_error(SharedString::default());
            ui.set_status_text("Loading message".into());
        }
        if let Err(error) = self.core.try_open_message(query) {
            self.state.borrow_mut().cancel_detail_submission();
            self.fail_detail(submit_error(error));
        }
    }

    fn fetch_message_content(self: &Rc<Self>, message_id: MessageId, account_id: i64) {
        {
            let mut task = self.content_task.borrow_mut();
            if task.as_ref().is_some_and(|task| !task.is_finished()) {
                return;
            }
            task.take();
        }
        let Some(account_key) = EntityKey::new(account_id).map(AccountKey::Account) else {
            self.finish_content_fetch_error(message_id, UserError::selection());
            return;
        };
        let Some(target) = self
            .catalog
            .borrow()
            .as_ref()
            .and_then(|catalog| catalog.operation_target(account_key))
        else {
            self.finish_content_fetch_error(message_id, UserError::selection());
            return;
        };
        let Some(request_id) = self.next_account_request_id() else {
            self.finish_content_fetch_error(message_id, UserError::account_session_limit());
            return;
        };
        let response =
            match self
                .core
                .try_account_operation(AccountOperation::FetchMessageContent {
                    request_id,
                    message_id,
                    account_id: target.account_id,
                    expected_generation: target.expected_generation,
                }) {
                Ok(response) => response,
                Err(failure) => {
                    self.finish_content_fetch_error(
                        message_id,
                        account_submit_error(failure.reason()),
                    );
                    return;
                }
            };
        let weak = Rc::downgrade(self);
        let task = slint::spawn_local(async move {
            let result = response.await;
            let Some(controller) = weak.upgrade() else {
                return;
            };
            controller.content_task.borrow_mut().take();
            match result {
                Ok(reply) if reply.request_id == request_id => match reply.result {
                    Ok(AccountOperationSuccess::MessageContentFetched {
                        message_id: fetched_message,
                        account_id,
                        generation,
                    }) if fetched_message == message_id
                        && account_id == target.account_id
                        && generation == target.expected_generation =>
                    {
                        if !controller.selected_message_matches(message_id) {
                            return;
                        }
                        if let Some(ui) = controller.ui.upgrade() {
                            ui.set_detail_content_pending(true);
                            ui.set_detail_content_error(SharedString::default());
                            ui.set_status_text("Opening downloaded body from local cache".into());
                        }
                        controller.refresh_selected_detail_from_cache(message_id);
                        controller.refresh_after_historical_sync();
                    }
                    Err(failure) => controller
                        .finish_content_fetch_error(message_id, account_operation_error(failure)),
                    Ok(_) => controller
                        .finish_content_fetch_error(message_id, UserError::account_result()),
                },
                Ok(_) => {
                    controller.finish_content_fetch_error(message_id, UserError::account_result())
                }
                Err(error) => {
                    controller.finish_content_fetch_error(message_id, account_response_error(error))
                }
            }
        });
        match task {
            Ok(task) => *self.content_task.borrow_mut() = Some(task),
            Err(_) => self.finish_content_fetch_error(message_id, UserError::account_runtime()),
        }
    }

    fn selected_message_matches(&self, message_id: MessageId) -> bool {
        self.ui.upgrade().is_some_and(|ui| {
            EntityKey::parse(ui.get_selected_id().as_str())
                .is_some_and(|key| key.get() == message_id.get())
        })
    }

    fn refresh_selected_detail_from_cache(&self, message_id: MessageId) {
        let Some(key) = EntityKey::new(message_id.get()) else {
            self.finish_content_fetch_error(message_id, UserError::selection());
            return;
        };
        let query = match self.state.borrow_mut().select_message(key) {
            Ok(query) => query,
            Err(error) => {
                self.finish_content_fetch_error(message_id, session_error(error));
                return;
            }
        };
        if let Err(error) = self.core.try_open_message(query) {
            self.state.borrow_mut().cancel_detail_submission();
            self.finish_content_fetch_error(message_id, submit_error(error));
        }
    }

    fn finish_content_fetch_error(&self, message_id: MessageId, error: UserError) {
        if !self.selected_message_matches(message_id) {
            return;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_detail_content_pending(false);
            ui.set_detail_content_error(error.detail.into());
            ui.set_status_text(error.title.into());
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

    fn refresh_after_historical_sync(&self) {
        self.pending_mailbox.borrow_mut().take();
        self.submit_mailbox_navigation(MailboxIntent::Refresh);
    }

    fn schedule_sync_refresh(self: &Rc<Self>, historical: bool) {
        let combined = self
            .pending_sync_refresh
            .get()
            .map_or(historical, |pending| pending && historical);
        self.pending_sync_refresh.set(Some(combined));
        let timer = self.sync_refresh_timer.clone();
        let controller = Rc::downgrade(self);
        timer.start(TimerMode::SingleShot, SYNC_REFRESH_COALESCE, move || {
            let Some(controller) = controller.upgrade() else {
                return;
            };
            let Some(historical) = controller.pending_sync_refresh.take() else {
                return;
            };
            let busy = controller
                .ui
                .upgrade()
                .is_some_and(|ui| ui.get_mailbox_loading() || ui.get_mailbox_loading_more());
            if busy {
                controller.schedule_sync_refresh(historical);
            } else if historical {
                controller.refresh_after_historical_sync();
            } else {
                controller.refresh_after_mutation();
            }
        });
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
        self.reset_remote_history_scope();
        self.reload_mailbox();
    }

    fn change_search(&self, query: &str) {
        let result = self.state.borrow_mut().set_search(query);
        match result {
            Ok(false) => self.restore_search_text(),
            Ok(true) => {
                self.restore_search_text();
                self.reset_remote_history_scope();
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
        self.reset_remote_history_scope();
        self.reload_mailbox();
    }

    fn reset_remote_history_scope(&self) {
        if let Some(task) = self.history_task.borrow_mut().take() {
            task.abort();
        }
        self.remote_append_pending.set(false);
        self.remote_history_more
            .set(self.state.borrow().can_load_remote_history());
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_loading_more(false);
        }
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

    fn open_composer(self: &Rc<Self>) {
        let Some(target) = self.active_compose_target() else {
            self.close_composer();
            self.notify_error(UserError::compose_account());
            return;
        };
        if !self.begin_compose_task("Loading saved draft") {
            return;
        }
        self.compose_target.set(Some(target));
        self.compose_identity.set(None);
        self.compose_dirty.set(false);
        self.window_close_pending.set(false);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_compose_to(SharedString::default());
            ui.set_compose_subject(SharedString::default());
            ui.set_compose_body(SharedString::default());
        }
        let response = match self
            .core
            .try_compose_operation(ComposeOperation::LoadLatest {
                account_id: target.account_id,
                expected_generation: target.expected_generation,
            }) {
            Ok(response) => response,
            Err(failure) => {
                self.finish_compose_task_with_error(compose_submit_error(failure.reason()));
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
                Ok(Ok(ComposeSuccess::Loaded(Some(draft)))) if draft.locked_for_delivery => {
                    controller.finish_compose_task();
                    controller.compose_identity.set(None);
                    controller.compose_dirty.set(false);
                    if let Some(ui) = controller.ui.upgrade() {
                        ui.set_composer_status("Previous message is already queued".into());
                        show_snackbar(
                            &ui,
                            "The previous message is already queued; this is a new draft",
                            false,
                            &controller.snackbar_timer,
                        );
                    }
                }
                Ok(Ok(ComposeSuccess::Loaded(Some(draft)))) => {
                    controller.finish_compose_task();
                    controller.compose_identity.set(Some(draft.identity));
                    controller.compose_dirty.set(false);
                    if let Some(ui) = controller.ui.upgrade() {
                        ui.set_compose_to(draft.to.as_ref().into());
                        ui.set_compose_subject(draft.subject.as_ref().into());
                        ui.set_compose_body(draft.body.as_ref().into());
                        ui.set_composer_status("Saved draft loaded".into());
                    }
                }
                Ok(Ok(ComposeSuccess::Loaded(None))) => {
                    controller.finish_compose_task();
                    controller.compose_dirty.set(false);
                    if let Some(ui) = controller.ui.upgrade() {
                        ui.set_composer_status("New draft".into());
                    }
                }
                Ok(Err(failure)) => controller.finish_compose_failure(failure),
                Ok(Ok(_)) => controller.finish_compose_task_with_error(UserError::compose_result()),
                Err(error) => {
                    controller.finish_compose_task_with_error(compose_response_error(error))
                }
            }
        });
        self.store_compose_task(task);
    }

    fn save_compose_draft(
        self: &Rc<Self>,
        to: SharedString,
        subject: SharedString,
        body: SharedString,
    ) {
        let Some(target) = self.compose_target.get() else {
            self.finish_compose_task_with_error(UserError::compose_account());
            return;
        };
        let input = match ComposeDraftInput::new(
            target.account_id,
            target.expected_generation,
            self.compose_identity.get(),
            to.as_str(),
            subject.as_str(),
            body.as_str(),
        ) {
            Ok(input) => input,
            Err(failure) => {
                self.finish_compose_failure(failure);
                return;
            }
        };
        if !self.begin_compose_task("Saving draft") {
            return;
        }
        let response = match self
            .core
            .try_compose_operation(ComposeOperation::SaveAndClose(input))
        {
            Ok(response) => response,
            Err(failure) => {
                self.finish_compose_task_with_error(compose_submit_error(failure.reason()));
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
                Ok(Ok(ComposeSuccess::Saved(_))) => {
                    controller.finish_compose_success("Draft saved");
                }
                Ok(Ok(ComposeSuccess::Discarded)) => {
                    controller.finish_compose_success("Empty draft discarded");
                }
                Ok(Err(failure)) => controller.finish_compose_failure(failure),
                Ok(Ok(_)) => controller.finish_compose_task_with_error(UserError::compose_result()),
                Err(error) => {
                    controller.finish_compose_task_with_error(compose_response_error(error))
                }
            }
        });
        self.store_compose_task(task);
    }

    fn queue_composed_message(
        self: &Rc<Self>,
        to: SharedString,
        subject: SharedString,
        body: SharedString,
    ) {
        let Some(target) = self.compose_target.get() else {
            self.finish_compose_task_with_error(UserError::compose_account());
            return;
        };
        let input = match ComposeDraftInput::new(
            target.account_id,
            target.expected_generation,
            self.compose_identity.get(),
            to.as_str(),
            subject.as_str(),
            body.as_str(),
        ) {
            Ok(input) => input,
            Err(failure) => {
                self.finish_compose_failure(failure);
                return;
            }
        };
        if !self.begin_compose_task("Queueing message") {
            return;
        }
        let response = match self
            .core
            .try_compose_operation(ComposeOperation::Queue(input))
        {
            Ok(response) => response,
            Err(failure) => {
                self.finish_compose_task_with_error(compose_submit_error(failure.reason()));
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
                Ok(Ok(ComposeSuccess::Queued { .. })) => {
                    controller.finish_compose_success("Message queued");
                    controller.reload_mailbox();
                    controller.load_outbox(false);
                }
                Ok(Ok(ComposeSuccess::Recovering)) => {
                    controller.finish_compose_success("Message queued for recovery");
                    controller.reload_mailbox();
                    controller.load_outbox(false);
                }
                Ok(Err(failure)) => controller.finish_compose_failure(failure),
                Ok(Ok(_)) => controller.finish_compose_task_with_error(UserError::compose_result()),
                Err(error) => {
                    controller.finish_compose_task_with_error(compose_response_error(error))
                }
            }
        });
        self.store_compose_task(task);
    }

    fn load_outbox(self: &Rc<Self>, open: bool) {
        if let Some(ui) = self.ui.upgrade()
            && open
        {
            ui.set_outbox_open(true);
        }
        if !self.begin_outbox_task(true, None) {
            return;
        }
        let response = match self
            .core
            .try_compose_operation(ComposeOperation::LoadOutbox)
        {
            Ok(response) => response,
            Err(failure) => {
                self.finish_outbox_task_with_error(compose_submit_error(failure.reason()));
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
                Ok(Ok(ComposeSuccess::OutboxLoaded(page))) => {
                    controller.apply_outbox_page(page);
                    controller.finish_outbox_task();
                }
                Ok(Err(failure)) => {
                    controller.finish_outbox_task_with_error(compose_failure_error(failure.kind))
                }
                Ok(Ok(_)) => controller.finish_outbox_task_with_error(UserError::compose_result()),
                Err(error) => {
                    controller.finish_outbox_task_with_error(compose_response_error(error))
                }
            }
        });
        self.store_outbox_task(task);
    }

    fn change_outbox(self: &Rc<Self>, key: &str, action: OutboxUiAction) {
        let Some((fence, state)) = self.outbox_fence(key) else {
            self.show_outbox_error(UserError::outbox_changed());
            return;
        };
        let valid = match action {
            OutboxUiAction::Retry | OutboxUiAction::ReleaseFailed => {
                state == OutboxState::PermanentFailure
            }
            OutboxUiAction::AssumeDelivered | OutboxUiAction::ReleaseUncertain => {
                state == OutboxState::Uncertain
            }
        };
        if !valid {
            self.show_outbox_error(UserError::outbox_changed());
            return;
        }
        let operation = match action {
            OutboxUiAction::Retry => ComposeOperation::RetryOutbox(fence),
            OutboxUiAction::ReleaseFailed => ComposeOperation::ReleaseFailedOutbox(fence),
            OutboxUiAction::AssumeDelivered => ComposeOperation::ResolveUncertainOutbox {
                fence,
                resolution: UncertainResolution::AssumeDelivered,
            },
            OutboxUiAction::ReleaseUncertain => ComposeOperation::ResolveUncertainOutbox {
                fence,
                resolution: UncertainResolution::Release,
            },
        };
        if !self.begin_outbox_task(false, Some(key)) {
            return;
        }
        let response = match self.core.try_compose_operation(operation) {
            Ok(response) => response,
            Err(failure) => {
                self.finish_outbox_task_with_error(compose_submit_error(failure.reason()));
                return;
            }
        };
        let core = self.core.clone();
        let weak = Rc::downgrade(self);
        let task = slint::spawn_local(async move {
            let result = response.await;
            let Some(controller) = weak.upgrade() else {
                return;
            };
            let applied = match (action, result) {
                (OutboxUiAction::Retry, Ok(Ok(ComposeSuccess::OutboxRetried { message_id })))
                    if message_id == fence.message_id =>
                {
                    true
                }
                (
                    OutboxUiAction::ReleaseFailed,
                    Ok(Ok(ComposeSuccess::OutboxReleased { message_id })),
                ) if message_id == fence.message_id => true,
                (
                    OutboxUiAction::AssumeDelivered,
                    Ok(Ok(ComposeSuccess::UncertainOutboxResolved {
                        message_id,
                        resolution: UncertainResolution::AssumeDelivered,
                    })),
                ) if message_id == fence.message_id => true,
                (
                    OutboxUiAction::ReleaseUncertain,
                    Ok(Ok(ComposeSuccess::UncertainOutboxResolved {
                        message_id,
                        resolution: UncertainResolution::Release,
                    })),
                ) if message_id == fence.message_id => true,
                (_, Ok(Err(failure))) => {
                    controller.finish_outbox_task_with_error(compose_failure_error(failure.kind));
                    return;
                }
                (_, Err(error)) => {
                    controller.finish_outbox_task_with_error(compose_response_error(error));
                    return;
                }
                _ => {
                    controller.finish_outbox_task_with_error(UserError::compose_result());
                    return;
                }
            };
            debug_assert!(applied);
            match request_outbox_page(core).await {
                Ok(page) => controller.apply_outbox_page(page),
                Err(error) => {
                    controller.finish_outbox_task_with_error(error);
                    return;
                }
            }
            controller.finish_outbox_task();
            if let Some(ui) = controller.ui.upgrade() {
                let message = match action {
                    OutboxUiAction::Retry => "Message queued to retry",
                    OutboxUiAction::ReleaseFailed | OutboxUiAction::ReleaseUncertain => {
                        "Message returned to Drafts"
                    }
                    OutboxUiAction::AssumeDelivered => "Message marked delivered",
                };
                ui.set_status_text(message.into());
                show_snackbar(&ui, message, false, &controller.snackbar_timer);
            }
            if action == OutboxUiAction::AssumeDelivered {
                controller.issue_accounts();
                controller.reload_mailbox();
            }
        });
        self.store_outbox_task(task);
    }

    fn cancel_outbox_attempt(self: &Rc<Self>, key: &str) {
        let Some((fence, OutboxState::InFlight)) = self.outbox_fence(key) else {
            self.show_outbox_error(UserError::outbox_changed());
            return;
        };
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        match self.core.cancel_outbox_attempt(fence.message_id) {
            OutboxCancelOutcome::Applied => {
                self.outbox_cancel_pending.set(true);
                ui.set_outbox_action_loading(true);
                ui.set_outbox_action_message_id(key.into());
                ui.set_outbox_error(SharedString::default());
                ui.set_status_text("Stopping the current SMTP attempt".into());
                show_snackbar(
                    &ui,
                    "Stopping this connection; durable delivery state will be preserved",
                    false,
                    &self.snackbar_timer,
                );
                let weak = Rc::downgrade(self);
                Timer::single_shot(Duration::from_millis(500), move || {
                    let Some(controller) = weak.upgrade() else {
                        return;
                    };
                    if controller.outbox_cancel_pending.get() {
                        controller.request_outbox_refresh();
                    }
                });
            }
            OutboxCancelOutcome::NotActive => {
                self.show_outbox_error(UserError::outbox_not_active());
                self.load_outbox(false);
            }
        }
    }

    fn open_outbox_credential_repair(&self, key: &str) {
        let Some(message_key) = EntityKey::parse(key) else {
            self.show_outbox_error(UserError::outbox_changed());
            return;
        };
        let Some((account_id, configuration_generation)) = self
            .outbox_snapshots
            .borrow()
            .iter()
            .find(|summary| {
                summary.message_id.get() == message_key.get()
                    && can_offer_credential_update(
                        summary.state,
                        summary.error_class,
                        summary.error_code.as_deref(),
                        true,
                    )
            })
            .map(|summary| (summary.account_id, summary.configuration_generation))
        else {
            self.show_outbox_error(UserError::outbox_changed());
            return;
        };
        let account_key = AccountKey::Account(
            EntityKey::new(account_id.get()).expect("persisted account IDs are positive"),
        );
        let matches_current_generation = self
            .catalog
            .borrow()
            .as_ref()
            .and_then(|catalog| catalog.operation_target(account_key))
            .is_some_and(|target| {
                target.account_id == account_id
                    && target.expected_generation == configuration_generation
            });
        if !matches_current_generation {
            self.show_outbox_error(UserError::outbox_changed());
            return;
        }

        let account_key = account_id.get().to_string();
        self.manage_account(&account_key);
        if let Some(ui) = self.ui.upgrade()
            && ui.get_managed_account_id().as_str() == account_key
        {
            ui.set_account_credential_open(true);
        }
    }

    fn outbox_fence(&self, key: &str) -> Option<(crate::core::OutboxActionFence, OutboxState)> {
        let key = EntityKey::parse(key)?;
        self.outbox_snapshots
            .borrow()
            .iter()
            .find(|summary| summary.message_id.get() == key.get())
            .map(|summary| (summary.action_fence(), summary.state))
    }

    fn begin_outbox_task(&self, loading: bool, key: Option<&str>) -> bool {
        if self.outbox_cancel_pending.get() || self.outbox_busy.replace(true) {
            self.show_outbox_error(UserError::outbox_busy());
            return false;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_outbox_loading(loading);
            ui.set_outbox_action_loading(!loading);
            ui.set_outbox_action_message_id(key.unwrap_or_default().into());
            ui.set_outbox_error(SharedString::default());
        }
        true
    }

    fn finish_outbox_task(self: &Rc<Self>) {
        self.outbox_busy.set(false);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_outbox_loading(false);
            ui.set_outbox_action_loading(false);
            ui.set_outbox_action_message_id(SharedString::default());
        }
        if self.outbox_refresh_pending.replace(false) {
            self.load_outbox(false);
        }
    }

    fn finish_outbox_task_with_error(self: &Rc<Self>, error: UserError) {
        self.finish_outbox_task();
        self.show_outbox_error(error);
    }

    fn show_outbox_error(&self, error: UserError) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_outbox_error(error.detail.into());
            ui.set_status_text(error.title.into());
        }
    }

    fn store_outbox_task(
        self: &Rc<Self>,
        task: Result<slint::JoinHandle<()>, slint::EventLoopError>,
    ) {
        match task {
            Ok(task) => *self.outbox_task.borrow_mut() = Some(task),
            Err(_) => self.finish_outbox_task_with_error(UserError::compose_runtime()),
        }
    }

    fn request_outbox_refresh(self: &Rc<Self>) {
        if self.outbox_cancel_pending.replace(false)
            && let Some(ui) = self.ui.upgrade()
        {
            ui.set_outbox_action_loading(false);
            ui.set_outbox_action_message_id(SharedString::default());
        }
        if self.outbox_busy.get() {
            self.outbox_refresh_pending.set(true);
        } else {
            self.load_outbox(false);
        }
    }

    fn apply_outbox_page(&self, page: OutboxSummaryPage) {
        let has_more = page.has_more;
        *self.outbox_snapshots.borrow_mut() = page.items;
        self.refresh_outbox_projection();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_outbox_has_more(has_more);
            ui.set_outbox_error(SharedString::default());
        }
    }

    fn refresh_outbox_projection(&self) {
        let catalog = self.catalog.borrow();
        let items = self
            .outbox_snapshots
            .borrow()
            .iter()
            .map(|summary| project_outbox_item(summary, catalog.as_ref()))
            .collect::<Vec<_>>();
        let count = i32::try_from(items.len()).unwrap_or(i32::MAX);
        self.outbox_model.set_vec(items);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_outbox_count(count);
        }
    }

    fn close_outbox(&self) {
        if self.outbox_busy.get() {
            return;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_outbox_open(false);
        }
    }

    fn active_compose_target(&self) -> Option<AccountOperationTarget> {
        let account = self.state.borrow().account();
        self.catalog.borrow().as_ref()?.operation_target(account)
    }

    fn bound_compose_input(&self, field: &str, value: SharedString) {
        let (limit, error) = match field {
            "to" => (
                COMPOSE_TO_FIELD_BYTE_LIMIT,
                "Recipient input reached its safety limit. Remove an address before adding another.",
            ),
            "subject" => (
                COMPOSE_SUBJECT_BYTE_LIMIT,
                "Subject input reached its safety limit. Shorten it before sending.",
            ),
            "body" => (
                COMPOSE_BODY_BYTE_LIMIT,
                "Message input reached the 1 MiB safety limit. Shorten it before adding more text.",
            ),
            _ => return,
        };
        if let Some(ui) = self.ui.upgrade()
            && ui.get_composer_open()
            && !ui.get_composer_loading()
        {
            self.compose_dirty.set(true);
            ui.set_composer_status("Changes not yet saved".into());
        }
        if value.as_str().len() <= limit {
            return;
        }
        let bounded = bounded_utf8_prefix(value.as_str(), limit);
        if let Some(ui) = self.ui.upgrade() {
            match field {
                "to" => ui.set_compose_to(bounded.into()),
                "subject" => ui.set_compose_subject(bounded.into()),
                "body" => ui.set_compose_body(bounded.into()),
                _ => unreachable!("compose field was validated above"),
            }
            ui.set_composer_error(error.into());
        }
    }

    fn begin_compose_task(&self, status: &'static str) -> bool {
        let mut task = self.compose_task.borrow_mut();
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            drop(task);
            self.show_compose_error(UserError::compose_busy());
            return false;
        }
        task.take();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_composer_loading(true);
            ui.set_composer_status(status.into());
            ui.set_composer_error(SharedString::default());
        }
        true
    }

    fn store_compose_task(&self, task: Result<slint::JoinHandle<()>, slint::EventLoopError>) {
        match task {
            Ok(task) => *self.compose_task.borrow_mut() = Some(task),
            Err(_) => self.finish_compose_task_with_error(UserError::compose_runtime()),
        }
    }

    fn finish_compose_task(&self) {
        self.compose_task.borrow_mut().take();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_composer_loading(false);
        }
    }

    fn finish_compose_success(&self, feedback: &'static str) {
        let exit_after_save = self.window_close_pending.replace(false);
        self.finish_compose_task();
        self.close_composer();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_status_text(feedback.into());
            show_snackbar(&ui, feedback, false, &self.snackbar_timer);
            if exit_after_save {
                ui.invoke_window_exit_approved();
            }
        }
    }

    fn finish_compose_failure(&self, failure: ComposeFailure) {
        self.compose_identity.set(failure.draft);
        self.finish_compose_task_with_error(compose_failure_error(failure.kind));
    }

    fn finish_compose_task_with_error(&self, error: UserError) {
        self.window_close_pending.set(false);
        self.finish_compose_task();
        self.show_compose_error(error);
    }

    fn show_compose_error(&self, error: UserError) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_composer_loading(false);
            ui.set_composer_status(error.title.into());
            ui.set_composer_error(error.detail.into());
            ui.set_status_text(error.title.into());
        }
    }

    fn close_composer(&self) {
        self.compose_identity.set(None);
        self.compose_target.set(None);
        self.compose_dirty.set(false);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_composer_open(false);
            ui.set_composer_loading(false);
            ui.set_compose_to(SharedString::default());
            ui.set_compose_subject(SharedString::default());
            ui.set_compose_body(SharedString::default());
            ui.set_composer_error(SharedString::default());
            ui.set_composer_status(SharedString::default());
        }
    }

    fn request_window_close(self: &Rc<Self>) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        if !ui.get_composer_open() || !self.compose_dirty.get() {
            ui.invoke_window_exit_approved();
            return;
        }
        if self.window_close_pending.get() {
            ui.set_composer_error(
                "Nivalis is saving this draft before closing. Keep the window open briefly.".into(),
            );
            return;
        }
        if self
            .compose_task
            .borrow()
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            ui.set_composer_status("Draft action still running".into());
            ui.set_composer_error(
                "Nivalis kept this window open so unsaved changes are not lost. Wait for the current draft action, then close again."
                    .into(),
            );
            ui.set_status_text("Window kept open to protect the draft".into());
            return;
        }

        self.window_close_pending.set(true);
        self.save_compose_draft(
            ui.get_compose_to(),
            ui.get_compose_subject(),
            ui.get_compose_body(),
        );
    }

    fn handle_outbox_status(self: &Rc<Self>, status: OutboxStatus) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let refresh = matches!(
            status,
            OutboxStatus::AttemptStarted { .. } | OutboxStatus::StateChanged { .. }
        );
        match status {
            OutboxStatus::AttemptStarted { .. } => {
                ui.set_status_text("Sending queued message".into());
            }
            OutboxStatus::StateChanged { state, .. } => match state {
                OutboxState::Reserved | OutboxState::Ready => {}
                OutboxState::InFlight => ui.set_status_text("Sending queued message".into()),
                OutboxState::RetryWait => {
                    ui.set_status_text("Message will retry automatically".into());
                    show_snackbar(
                        &ui,
                        "Message was not sent; Nivalis will retry automatically",
                        false,
                        &self.snackbar_timer,
                    );
                }
                OutboxState::Uncertain => {
                    ui.set_status_text("Message delivery needs review".into());
                    show_snackbar(
                        &ui,
                        "The server response was unclear; Nivalis will not send a duplicate",
                        false,
                        &self.snackbar_timer,
                    );
                }
                OutboxState::PermanentFailure => {
                    ui.set_status_text("Message could not be sent".into());
                    show_snackbar(
                        &ui,
                        "Message could not be sent; check the account settings before retrying",
                        false,
                        &self.snackbar_timer,
                    );
                }
                OutboxState::Delivered => {
                    ui.set_status_text("Message sent".into());
                    show_snackbar(&ui, "Message sent", false, &self.snackbar_timer);
                    self.issue_accounts();
                    self.reload_mailbox();
                }
            },
            OutboxStatus::Fault { kind, .. } => {
                let error = match kind {
                    OutboxDriverFault::Database => UserError::compose_database(),
                    OutboxDriverFault::ContentStorage => UserError::compose_storage(),
                    OutboxDriverFault::Credential => UserError::compose_credentials(),
                    OutboxDriverFault::InvalidSubmission => UserError::compose_input(),
                };
                ui.set_status_text(error.title.into());
                show_snackbar(&ui, error.detail, false, &self.snackbar_timer);
            }
        }
        if refresh {
            self.request_outbox_refresh();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_account(
        self: &Rc<Self>,
        name: SharedString,
        address: SharedString,
        login: SharedString,
        imap_host: SharedString,
        imap_port: SharedString,
        smtp_host: SharedString,
        smtp_port: SharedString,
        password: SharedString,
    ) {
        let Some(imap_port) = parse_mail_port(imap_port.as_str()) else {
            self.show_account_error(UserError::account_imap_port());
            return;
        };
        let Some(smtp_port) = parse_mail_port(smtp_port.as_str()) else {
            self.show_account_error(UserError::account_smtp_port());
            return;
        };
        let draft = match AccountConfigDraft::new(
            name.as_str(),
            address.as_str(),
            login.as_str(),
            imap_host.as_str(),
            imap_port,
            smtp_host.as_str(),
            smtp_port,
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

    fn update_account_credential(self: &Rc<Self>, key: &str, password: SharedString) {
        let Some(target) = self.account_operation_target(key) else {
            self.show_account_error(UserError::selection());
            return;
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
        if !self.begin_account_task("Saving app password", false) {
            return;
        }
        let response = match self
            .core
            .try_account_operation(AccountOperation::RetryCredential {
                request_id,
                account_id: target.account_id,
                expected_generation: target.expected_generation,
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
            let result = response.await;
            let Some(controller) = weak.upgrade() else {
                return;
            };
            match result {
                Ok(reply)
                    if reply.request_id == request_id
                        && matches!(
                            reply.result,
                            Ok(AccountOperationSuccess::Configured {
                                account_id,
                                generation,
                            }) if account_id == target.account_id
                                && generation == target.expected_generation
                        ) =>
                {
                    controller.finish_account_task();
                    if let Some(ui) = controller.ui.upgrade() {
                        ui.set_account_credential_open(false);
                        ui.set_account_status_open(false);
                        ui.set_account_operation_error(SharedString::default());
                        ui.set_status_text("App password updated".into());
                        show_snackbar(
                            &ui,
                            "App password updated; retry the message from Outbox",
                            false,
                            &controller.snackbar_timer,
                        );
                    }
                    controller.issue_accounts();
                    controller.request_outbox_refresh();
                }
                Ok(reply) if reply.request_id == request_id => match reply.result {
                    Err(failure) => {
                        controller.finish_account_task_with_error(account_operation_error(failure))
                    }
                    Ok(_) => controller.finish_account_task_with_error(UserError::account_result()),
                },
                Ok(_) => controller.finish_account_task_with_error(UserError::account_result()),
                Err(error) => {
                    controller.finish_account_task_with_error(account_response_error(error))
                }
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
                        historical,
                    }) if account_id == target.account_id
                        && generation == target.expected_generation =>
                    {
                        let feedback = background_sync_feedback(imported, has_more, historical);
                        if controller.selected_account_matches(account_id)
                            && controller.state.borrow().can_load_remote_history()
                        {
                            controller.remote_history_more.set(has_more);
                        }
                        controller.finish_account_task();
                        if let Some(ui) = controller.ui.upgrade() {
                            ui.set_status_text(feedback.clone().into());
                            show_snackbar(&ui, &feedback, false, &controller.snackbar_timer);
                        }
                        controller.issue_accounts();
                        controller.schedule_sync_refresh(historical);
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
        if let Some(task) = self.compose_task.borrow_mut().take() {
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
        let compose_enabled = self
            .catalog
            .borrow()
            .as_ref()
            .and_then(|catalog| catalog.operation_target(account_key))
            .is_some();
        let item = self
            .catalog
            .borrow()
            .as_ref()
            .and_then(|catalog| catalog.active_item(account_key));
        let Some(item) = item else {
            if let Some(ui) = self.ui.upgrade() {
                ui.set_compose_enabled(false);
            }
            self.notify_error(UserError::selection());
            return;
        };
        if let Some(ui) = self.ui.upgrade() {
            ui.set_compose_enabled(compose_enabled);
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
        if let Some(task) = self.content_task.borrow_mut().take() {
            task.abort();
        }
        self.state.borrow_mut().clear_selection();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_selected_id(SharedString::default());
            ui.set_selected_mail(MailDetail::default());
            ui.set_detail_loading(false);
            ui.set_detail_error(false);
            ui.set_detail_content_pending(false);
            ui.set_detail_content_error(SharedString::default());
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
        self.reset_remote_history_scope();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_has_accounts(false);
            ui.set_compose_enabled(false);
            ui.set_initial_loading(false);
            ui.set_mail_actions_enabled(false);
        }
        self.fail_mailbox(error);
    }

    fn fail_mailbox(&self, error: UserError) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_mailbox_loading(false);
            ui.set_mailbox_has_more(false);
            ui.set_mailbox_loading_more(false);
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
            ui.set_detail_content_pending(false);
            ui.set_detail_content_error(SharedString::default());
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
        if let Some(task) = self.history_task.borrow_mut().take() {
            task.abort();
        }
        if let Some(task) = self.content_task.borrow_mut().take() {
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
            ui.set_compose_enabled(false);
            ui.set_composer_loading(false);
            if ui.get_composer_open() {
                ui.set_composer_status("Draft service stopped".into());
                ui.set_composer_error(
                    "Restart Nivalis to reopen the last durably saved draft.".into(),
                );
            }
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
        (0, true) => "More messages are available; scroll down to load them".to_owned(),
        (1, false) => "Imported 1 new message".to_owned(),
        (1, true) => "Imported 1 new message; scroll down for more".to_owned(),
        (count, false) => format!("Imported {count} new messages"),
        (count, true) => format!("Imported {count} new messages; scroll down for more"),
    }
}

fn background_sync_feedback(imported: u8, has_more: bool, historical: bool) -> String {
    if !historical {
        return sync_success_feedback(imported, has_more);
    }
    match (imported, has_more) {
        (0, false) => "Mail history is fully synced".to_owned(),
        (0, true) => "Older messages are available; scroll down to load them".to_owned(),
        (1, false) => "Downloaded the last older message".to_owned(),
        (1, true) => "Downloaded 1 older message; scroll down for more".to_owned(),
        (count, false) => format!("Downloaded the last {count} older messages"),
        (count, true) => format!("Downloaded {count} older messages; scroll down for more"),
    }
}

fn should_refresh_synced_mailbox(
    imported: u8,
    shows_inbox_updates: bool,
    mailbox_loading: bool,
    mailbox_loading_more: bool,
) -> bool {
    imported > 0 && shows_inbox_updates && !mailbox_loading && !mailbox_loading_more
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

    const fn account_imap_port() -> Self {
        Self {
            title: "Check the IMAP port",
            detail: "Enter a port from 1 to 65535. Secure IMAP normally uses 993.",
        }
    }

    const fn account_smtp_port() -> Self {
        Self {
            title: "Check the SMTP port",
            detail: "Enter a port from 1 to 65535. Port 465 uses implicit TLS; other ports require STARTTLS.",
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

    const fn compose_account() -> Self {
        Self {
            title: "Choose one account to send from",
            detail: "Select a configured account instead of All inboxes, then write the message.",
        }
    }

    const fn compose_busy() -> Self {
        Self {
            title: "A draft action is already running",
            detail: "Wait for the current save or queue operation to finish, then try again.",
        }
    }

    const fn compose_result() -> Self {
        Self {
            title: "Draft result could not be verified",
            detail: "Keep this window open and try again. Nivalis will not report the message as queued without durable confirmation.",
        }
    }

    const fn compose_runtime() -> Self {
        Self {
            title: "Draft action could not start",
            detail: "The interface event loop is unavailable. Restart Nivalis and reopen the draft.",
        }
    }

    const fn compose_input() -> Self {
        Self {
            title: "Check the message fields",
            detail: "Enter valid comma-separated recipient addresses and remove control characters from the subject.",
        }
    }

    const fn compose_storage() -> Self {
        Self {
            title: "Draft file could not be stored",
            detail: "Check available disk space and access to the Nivalis data directory, then try again.",
        }
    }

    const fn compose_database() -> Self {
        Self {
            title: "Draft database is unavailable",
            detail: "Keep the message open and try again. Restart Nivalis if the problem continues.",
        }
    }

    const fn compose_credentials() -> Self {
        Self {
            title: "Mail password is unavailable",
            detail: "Unlock the system credential store or update the account, then retry the message.",
        }
    }

    const fn outbox_busy() -> Self {
        Self {
            title: "An Outbox action is already running",
            detail: "Wait for the current Outbox action to finish, then try again.",
        }
    }

    const fn outbox_changed() -> Self {
        Self {
            title: "Outbox state changed",
            detail: "Reload the Outbox and use the actions shown for the current delivery state.",
        }
    }

    const fn outbox_not_active() -> Self {
        Self {
            title: "No SMTP connection is active",
            detail: "The attempt finished or has not opened its connection yet. Reload the Outbox to see its durable state.",
        }
    }
}

fn compose_failure_error(kind: ComposeFailureKind) -> UserError {
    match kind {
        ComposeFailureKind::InvalidInput => UserError::compose_input(),
        ComposeFailureKind::ResourceLimit => UserError {
            title: "Message exceeds a safety limit",
            detail: "Shorten the body, subject, or recipient list, then try again.",
        },
        ComposeFailureKind::Conflict => UserError {
            title: "Draft changed before it was saved",
            detail: "Close and reopen the composer to load the latest durable draft before editing again.",
        },
        ComposeFailureKind::Configuration => UserError {
            title: "Outgoing mail is not configured",
            detail: "Remove and add this account with its SMTP server settings before sending. The current draft can still be saved locally.",
        },
        ComposeFailureKind::NotFound => UserError {
            title: "Draft is no longer available",
            detail: "Close and reopen the composer to start from the current mailbox state.",
        },
        ComposeFailureKind::Storage => UserError::compose_storage(),
        ComposeFailureKind::Database => UserError::compose_database(),
        ComposeFailureKind::Unavailable => UserError::compose_result(),
    }
}

fn compose_submit_error(error: ComposeOperationSubmitError) -> UserError {
    match error {
        ComposeOperationSubmitError::Busy => UserError::compose_busy(),
        ComposeOperationSubmitError::Closed => UserError::compose_result(),
    }
}

fn compose_response_error(_error: ComposeOperationResponseError) -> UserError {
    UserError::compose_result()
}

async fn request_outbox_page(core: CoreHandle) -> Result<OutboxSummaryPage, UserError> {
    let response = core
        .try_compose_operation(ComposeOperation::LoadOutbox)
        .map_err(|failure| compose_submit_error(failure.reason()))?;
    match response.await {
        Ok(Ok(ComposeSuccess::OutboxLoaded(page))) => Ok(page),
        Ok(Err(failure)) => Err(compose_failure_error(failure.kind)),
        Ok(Ok(_)) => Err(UserError::compose_result()),
        Err(error) => Err(compose_response_error(error)),
    }
}

fn project_outbox_item(summary: &OutboxSummary, catalog: Option<&AccountCatalog>) -> OutboxItem {
    let account_key = EntityKey::new(summary.account_id.get()).map(AccountKey::Account);
    let account_label = account_key
        .and_then(|key| catalog?.active_item(key))
        .map(|item| item.name)
        .unwrap_or_else(|| format!("Mail account {}", summary.account_id.get()).into());
    let account_generation_matches = account_key
        .and_then(|key| catalog?.operation_target(key))
        .is_some_and(|target| target.expected_generation == summary.configuration_generation);
    let recipient_count = summary
        .recipients
        .to_count
        .saturating_add(summary.recipients.cc_count)
        .saturating_add(summary.recipients.bcc_count);
    let recipients = match (
        summary.recipients.first_to_address.as_deref(),
        recipient_count,
    ) {
        (Some(first), 0 | 1) => SharedString::from(first),
        (Some(first), count) => format!("{first} and {} more", count - 1).into(),
        (None, 1) => "1 recipient".into(),
        (None, count) => format!("{count} recipients").into(),
    };
    let (state, status, attempt_detail) = match summary.state {
        OutboxState::Reserved => (
            "reserved",
            "Preparing",
            "Building the private outbound message file.",
        ),
        OutboxState::Ready => (
            "ready",
            "Queued",
            "Waiting for the bounded sender; no idle SMTP connection is retained.",
        ),
        OutboxState::InFlight => (
            "in_flight",
            "Sending",
            "The current SMTP connection is active.",
        ),
        OutboxState::RetryWait => (
            "retry_wait",
            "Retry scheduled",
            "The previous attempt ended before confirmed delivery; Nivalis will retry automatically.",
        ),
        OutboxState::Uncertain => (
            "uncertain",
            "Review needed",
            "Server acceptance could not be confirmed; automatic resend is disabled.",
        ),
        OutboxState::PermanentFailure => (
            "permanent_failure",
            "Not sent",
            permanent_outbox_detail(summary.error_class),
        ),
        OutboxState::Delivered => (
            "delivered",
            "Delivered",
            "Delivery was confirmed and local cleanup is pending.",
        ),
    };
    let attempt_detail = if matches!(
        summary.state,
        OutboxState::InFlight | OutboxState::RetryWait
    ) {
        format!("{attempt_detail} Attempt {}.", summary.attempt_count).into()
    } else {
        attempt_detail.into()
    };
    OutboxItem {
        message_id: summary.message_id.get().to_string().into(),
        account_label,
        recipients,
        subject: summary.subject.as_ref().into(),
        state: state.into(),
        status: status.into(),
        attempt_detail,
        can_update_credential: can_offer_credential_update(
            summary.state,
            summary.error_class,
            summary.error_code.as_deref(),
            account_generation_matches,
        ),
    }
}

fn can_offer_credential_update(
    state: OutboxState,
    error_class: Option<OutboxErrorClass>,
    error_code: Option<&str>,
    account_generation_matches: bool,
) -> bool {
    state == OutboxState::PermanentFailure
        && error_class == Some(OutboxErrorClass::Authentication)
        && error_code == Some("smtp_authentication_rejected")
        && account_generation_matches
}

fn permanent_outbox_detail(error_class: Option<OutboxErrorClass>) -> &'static str {
    match error_class {
        Some(OutboxErrorClass::Authentication) => {
            "The server rejected the account credentials. Check the account before retrying."
        }
        Some(OutboxErrorClass::Configuration) => {
            "Outgoing server settings changed or could not be verified. Return to Drafts after fixing the account."
        }
        Some(OutboxErrorClass::Protocol) => {
            "The server did not complete a supported SMTP exchange. Check the account before retrying."
        }
        Some(OutboxErrorClass::Network | OutboxErrorClass::RateLimit) => {
            "Automatic retries reached a safety limit. Check connectivity before retrying."
        }
        Some(OutboxErrorClass::Permanent | OutboxErrorClass::Ambiguous) | None => {
            "Delivery stopped safely. Retry it or return the message to Drafts."
        }
    }
}

fn bounded_utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn parse_mail_port(value: &str) -> Option<u16> {
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
            title: "Check the mail server names",
            detail: "Enter valid IMAP and SMTP server names. Nivalis always verifies their certificates.",
        },
        AccountValidationError::Port => UserError::account_imap_port(),
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

fn foreground_feedback_active(ui: &AppWindow) -> bool {
    ui.get_snackbar_visible()
        || ui.get_account_operation_loading()
        || ui.get_sync_loading()
        || ui.get_mutation_loading()
        || ui.get_outbox_action_loading()
        || ui.get_composer_loading()
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
            detail: "The connection succeeded, but Nivalis could not safely process the IMAP response. Update Nivalis and sync Inbox again; if it continues, report a provider compatibility issue.",
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

    fn verify_f5_sync_shortcut() {
        let ui = AppWindow::new().expect("create shortcut test window");
        let synced_account = Rc::new(RefCell::new(None));
        let synced_account_for_callback = synced_account.clone();
        ui.on_sync_account(move |account_id| {
            *synced_account_for_callback.borrow_mut() = Some(account_id.to_string());
        });
        ui.set_active_account_id("account-7".into());

        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::F5.into(),
            });
        assert_eq!(synced_account.borrow().as_deref(), Some("account-7"));

        *synced_account.borrow_mut() = None;
        ui.set_sync_loading(true);
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::F5.into(),
            });
        assert!(synced_account.borrow().is_none());

        ui.set_sync_loading(false);
        ui.set_settings_open(true);
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::F5.into(),
            });
        assert!(synced_account.borrow().is_none());
    }

    #[test]
    fn compose_input_bound_preserves_utf8_boundaries() {
        assert_eq!(bounded_utf8_prefix("plain", 5), "plain");
        assert_eq!(bounded_utf8_prefix("a雪b", 4), "a雪");
        assert_eq!(bounded_utf8_prefix("雪", 2), "");
        assert_eq!(bounded_utf8_prefix("body", 0), "");
    }

    #[test]
    fn credential_update_is_offered_only_for_current_smtp_rejection() {
        assert!(can_offer_credential_update(
            OutboxState::PermanentFailure,
            Some(OutboxErrorClass::Authentication),
            Some("smtp_authentication_rejected"),
            true,
        ));
        assert!(!can_offer_credential_update(
            OutboxState::PermanentFailure,
            Some(OutboxErrorClass::Authentication),
            Some("credential_unavailable"),
            true,
        ));
        assert!(!can_offer_credential_update(
            OutboxState::PermanentFailure,
            Some(OutboxErrorClass::Authentication),
            Some("smtp_authentication_rejected"),
            false,
        ));
        assert!(!can_offer_credential_update(
            OutboxState::RetryWait,
            Some(OutboxErrorClass::Authentication),
            Some("smtp_authentication_rejected"),
            true,
        ));
    }

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

        fn seed_failed_outbox(&self) -> (i64, PathBuf) {
            let mut connection = Connection::open(&self.path).expect("open outbox fixture");
            connection
                .execute_batch(
                    "PRAGMA foreign_keys = ON;
                     INSERT INTO accounts
                         (id, provider, remote_key, name, address, state, accent_rgb)
                     VALUES
                         (1, 'imap', 'outbox-account', 'Send account',
                          'sender@example.test', 'active', 0);
                     INSERT INTO account_connections
                         (account_id, credential_key, auth_kind, login_name,
                          imap_host, imap_port, smtp_host, smtp_port,
                          smtp_security, smtp_state)
                     VALUES
                         (1, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'app_password',
                          'sender@example.test', 'imap.example.test', 993,
                          'smtp.example.test', 465, 'implicit_tls', 'configured');",
                )
                .expect("seed outbox account");

            let account_id = sqlite::AccountId::new(1).expect("valid outbox account id");
            let generation = sqlite::AccountGeneration::new(1).expect("valid outbox generation");
            let draft = sqlite::create_draft(
                &mut connection,
                &sqlite::NewDraft::new(
                    account_id,
                    generation,
                    "local:controller-outbox",
                    "Persistent send failure",
                    "Draft body",
                    "Draft body",
                    crate::content::FileKey::parse("body/11111111111111111111111111111111.txt")
                        .expect("valid body key"),
                    10,
                    vec![
                        sqlite::DraftRecipient::new("recipient@example.test", "Recipient")
                            .expect("valid draft recipient"),
                    ],
                    1_700_000_000_000,
                )
                .expect("valid outbox draft"),
            )
            .expect("create outbox draft");
            let reservation = sqlite::reserve_outbox(
                &mut connection,
                &sqlite::OutboxReserveRequest::new(
                    draft.message_id,
                    account_id,
                    generation,
                    draft.revision,
                    sqlite::OutboxReservationToken::new([0x5a; 16]),
                    "<controller-outbox@example.test>",
                    vec![
                        sqlite::OutboxRecipient::new(
                            sqlite::RecipientKind::To,
                            "recipient@example.test",
                            "Recipient",
                        )
                        .expect("valid outbox recipient"),
                    ],
                    1_700_000_000_001,
                    1_700_000_001_001,
                )
                .expect("valid outbox reservation"),
            )
            .expect("reserve outbox draft");

            let staging = crate::content::ContentStaging::open(self.path.with_file_name("content"))
                .expect("open controller content root");
            let wire = b"From: sender@example.test\r\nTo: recipient@example.test\r\n\r\nbody\r\n";
            let staged = staging
                .stage_reader_at(&reservation.file_key, wire.as_slice(), 8 * 1024 * 1024)
                .expect("stage controller outbound MIME");
            let wire_byte_count = staged.byte_count();
            let mut published = staged.publish().expect("publish controller outbound MIME");
            published.retain();
            let mime_path = self
                .path
                .with_file_name("content")
                .join(reservation.file_key.as_str());

            assert_eq!(
                sqlite::finalize_outbox(
                    &mut connection,
                    &reservation,
                    wire_byte_count,
                    1_700_000_000_002,
                )
                .expect("finalize outbox fixture"),
                sqlite::OutboxReportOutcome::Applied(sqlite::OutboxState::Ready)
            );
            let sqlite::OutboxClaimOutcome::Claimed(claim) =
                sqlite::claim_next_outbox(&mut connection, 1_700_000_000_003)
                    .expect("claim outbox fixture")
            else {
                panic!("expected controller outbox claim");
            };
            assert_eq!(
                sqlite::report_outbox(
                    &mut connection,
                    claim.lease,
                    &sqlite::OutboxReport::permanent_failure(
                        sqlite::OutboxErrorClass::Authentication,
                        "smtp_authentication_rejected",
                    )
                    .expect("valid permanent failure"),
                    1_700_000_000_004,
                )
                .expect("terminalize outbox fixture"),
                sqlite::OutboxReportOutcome::Applied(sqlite::OutboxState::PermanentFailure)
            );
            (draft.message_id.get(), mime_path)
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

    fn mailbox_ready(ui: &AppWindow, row_count: usize) -> bool {
        !ui.get_initial_loading()
            && !ui.get_mailbox_loading()
            && !ui.get_mailbox_loading_more()
            && !ui.get_mutation_loading()
            && !ui.get_mailbox_error()
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
    fn compose_failures_are_actionable_and_do_not_disclose_internal_messages() {
        for kind in [
            ComposeFailureKind::InvalidInput,
            ComposeFailureKind::ResourceLimit,
            ComposeFailureKind::Conflict,
            ComposeFailureKind::Configuration,
            ComposeFailureKind::NotFound,
            ComposeFailureKind::Storage,
            ComposeFailureKind::Database,
            ComposeFailureKind::Unavailable,
        ] {
            let error = compose_failure_error(kind);
            assert!(!error.title.is_empty());
            assert!(!error.detail.is_empty());
            assert!(!error.detail.contains("secret database message"));
        }

        let failure = ComposeFailure {
            kind: ComposeFailureKind::Database,
            message: "secret database message".into(),
            draft: None,
        };
        let error = compose_failure_error(failure.kind);
        assert!(!error.title.contains(failure.message.as_ref()));
        assert!(!error.detail.contains(failure.message.as_ref()));
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
            "More messages are available; scroll down to load them"
        );
        assert_eq!(sync_success_feedback(1, false), "Imported 1 new message");
        assert_eq!(
            sync_success_feedback(16, true),
            "Imported 16 new messages; scroll down for more"
        );
        assert_eq!(
            background_sync_feedback(16, true, true),
            "Downloaded 16 older messages; scroll down for more"
        );
        assert_eq!(
            background_sync_feedback(0, false, true),
            "Mail history is fully synced"
        );
        assert!(should_refresh_synced_mailbox(1, true, false, false));
        assert!(should_refresh_synced_mailbox(16, true, false, false));
        assert!(!should_refresh_synced_mailbox(0, true, false, false));
        assert!(!should_refresh_synced_mailbox(16, true, false, true));
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
        assert_eq!(parse_mail_port("993"), Some(993));
        assert_eq!(parse_mail_port("465"), Some(465));
        for invalid in ["", "0", " 993", "993 ", "-1", "65536", "imap"] {
            assert_eq!(parse_mail_port(invalid), None, "accepted {invalid:?}");
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
            (InboxSyncFailureKind::Protocol, "provider compatibility"),
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

        let protocol = inbox_sync_error(InboxSyncFailureKind::Protocol);
        assert!(protocol.detail.contains("connection succeeded"));
        assert!(!protocol.detail.contains("selected port"));
    }

    #[test]
    fn production_controller_drives_sqlite_success_empty_and_error_states() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        verify_f5_sync_shortcut();

        let database = TestDatabase::new("mailbox");
        database.seed_bounded_mailbox();
        let mut harness = ControllerHarness::start(&database.path);
        harness.drive_until("initial bounded mailbox", |ui| mailbox_ready(ui, 50));

        assert!(harness.ui.get_has_accounts());
        assert!(harness.ui.get_mail_actions_enabled());
        assert!(!harness.ui.get_compose_enabled());
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
            "smtp.example.test".into(),
            "465".into(),
            "not-retained".into(),
        );
        assert_eq!(
            harness.ui.get_account_operation_error().as_str(),
            "Enter a port from 1 to 65535. Secure IMAP normally uses 993."
        );
        assert!(!harness.ui.get_account_operation_loading());

        harness.ui.invoke_add_account(
            "Personal".into(),
            "user@example.test".into(),
            "user@example.test".into(),
            "imap.example.test".into(),
            "993".into(),
            "smtp.example.test".into(),
            "0".into(),
            "not-retained".into(),
        );
        assert_eq!(
            harness.ui.get_account_operation_error().as_str(),
            "Enter a port from 1 to 65535. Port 465 uses implicit TLS; other ports require STARTTLS."
        );
        assert!(!harness.ui.get_account_operation_loading());

        assert_eq!(harness.ui.get_message_total(), 51);
        assert!(harness.ui.get_mailbox_has_more());
        assert!(model_contains(&harness.ui, "51"));

        harness.ui.invoke_load_more_mail();
        harness.drive_until("continuous mailbox append", |ui| mailbox_ready(ui, 51));
        assert!(model_contains(&harness.ui, "1"));
        assert!(model_contains(&harness.ui, "51"));
        assert!(!harness.ui.get_mailbox_has_more());

        harness.ui.invoke_switch_account("1".into());
        harness.drive_until("single-account mailbox", |ui| mailbox_ready(ui, 1));
        assert_eq!(harness.ui.get_active_account_id().as_str(), "1");
        assert!(harness.ui.get_compose_enabled());
        assert!(model_contains(&harness.ui, "1"));

        harness.ui.invoke_switch_account("".into());
        harness.drive_until("all-account mailbox", |ui| mailbox_ready(ui, 50));
        assert!(harness.ui.get_active_account_id().is_empty());
        assert!(!harness.ui.get_compose_enabled());

        harness.ui.set_search_query("message 49".into());
        harness.ui.invoke_query_mail("message 49".into());
        thread::sleep(Duration::from_millis(200));
        slint::platform::update_timers_and_animations();
        harness.drive_until("FTS mailbox result", |ui| mailbox_ready(ui, 1));
        assert!(model_contains(&harness.ui, "49"));

        harness.ui.set_search_query("".into());
        harness.ui.invoke_query_mail("".into());
        thread::sleep(Duration::from_millis(200));
        slint::platform::update_timers_and_animations();
        harness.drive_until("cleared FTS mailbox", |ui| mailbox_ready(ui, 50));

        harness.ui.set_detail_open(true);
        harness.ui.invoke_select_mail("51".into());
        harness.drive_until("selected message detail", |ui| {
            !ui.get_detail_loading() && ui.get_selected_mail().id.as_str() == "51"
        });
        assert_eq!(harness.ui.get_selected_mail().body.as_str().len(), 65_536);

        assert!(model_summary(&harness.ui, "51").starred);
        harness.ui.invoke_toggle_star("51".into());
        harness.drive_until("star mutation refresh", |ui| {
            mailbox_ready(ui, 50) && !model_summary(ui, "51").starred
        });

        assert!(!model_summary(&harness.ui, "50").unread);
        harness.ui.invoke_mark_unread("50".into());
        harness.drive_until("unread mutation refresh", |ui| {
            mailbox_ready(ui, 50) && model_summary(ui, "50").unread
        });

        harness.ui.invoke_archive("51".into());
        harness.drive_until("archive mutation refresh", |ui| {
            mailbox_ready(ui, 50) && !model_contains(ui, "51")
        });

        harness.ui.invoke_delete_mail("49".into());
        harness.drive_until("Trash mutation refresh", |ui| {
            mailbox_ready(ui, 49) && !model_contains(ui, "49") && ui.get_snackbar_can_undo()
        });
        harness.ui.invoke_undo_delete();
        harness.drive_until("Trash undo refresh", |ui| {
            mailbox_ready(ui, 50) && model_contains(ui, "49") && !ui.get_snackbar_can_undo()
        });

        harness.ui.invoke_delete_mail("48".into());
        harness.drive_until("permanent-delete setup", |ui| {
            mailbox_ready(ui, 49) && !model_contains(ui, "48")
        });
        harness.ui.invoke_filter_folder("Trash".into());
        harness.drive_until("Trash folder", |ui| mailbox_ready(ui, 1));
        assert!(model_contains(&harness.ui, "48"));
        harness.ui.invoke_delete_mail("48".into());
        harness.drive_until("permanent deletion", |ui| mailbox_ready(ui, 0));

        harness.ui.invoke_filter_folder("Archive".into());
        harness.drive_until("Archive folder", |ui| mailbox_ready(ui, 1));
        assert!(model_contains(&harness.ui, "51"));

        harness.ui.invoke_switch_account("51".into());
        harness.drive_until("configured-account Archive", |ui| mailbox_ready(ui, 1));
        crate::platform::install_window_handlers(&harness.ui);
        let close_approved = Rc::new(Cell::new(false));
        let approved_callback = close_approved.clone();
        harness.ui.on_window_exit_approved(move || {
            approved_callback.set(true);
            slint::quit_event_loop().expect("stop protected close test loop");
        });
        harness.ui.set_composer_open(true);
        harness.ui.invoke_open_composer();

        let phase = Rc::new(Cell::new(0_u8));
        let poll_phase = phase.clone();
        let poll_ui = harness.ui.as_weak();
        let poll_controller = harness.controller.clone();
        let poll_close_approved = close_approved.clone();
        let poll_timer = Timer::default();
        poll_timer.start(TimerMode::Repeated, Duration::from_millis(10), move || {
            let Some(ui) = poll_ui.upgrade() else {
                return;
            };
            if poll_phase.get() == 0 && !ui.get_composer_loading() {
                assert!(
                    ui.get_composer_error().is_empty(),
                    "composer failed to load: {} / {}",
                    ui.get_composer_status(),
                    ui.get_composer_error()
                );
                ui.set_compose_to("not-an-address".into());
                ui.set_compose_subject("Protected close draft".into());
                ui.set_compose_body("This body must be durable before the window closes.".into());
                ui.invoke_compose_input_edited(
                    "body".into(),
                    "This body must be durable before the window closes.".into(),
                );
                assert!(poll_controller.compose_dirty.get());

                ui.window()
                    .dispatch_event(slint::platform::WindowEvent::CloseRequested);
                assert!(!poll_close_approved.get());
                assert!(ui.get_composer_open());
                assert!(!ui.get_composer_loading());
                assert!(!poll_controller.window_close_pending.get());
                assert_eq!(
                    ui.get_composer_status().as_str(),
                    "Check the message fields"
                );
                assert!(ui.get_composer_error().contains("valid comma-separated"));

                ui.set_compose_to("recipient@example.test".into());
                ui.invoke_compose_input_edited("to".into(), "recipient@example.test".into());
                ui.window()
                    .dispatch_event(slint::platform::WindowEvent::CloseRequested);
                assert!(!poll_close_approved.get());
                assert!(ui.get_composer_open());
                assert!(ui.get_composer_loading());
                assert_eq!(ui.get_composer_status().as_str(), "Saving draft");
                poll_phase.set(1);
            }
        });
        let timed_out = close_approved.clone();
        Timer::single_shot(Duration::from_secs(5), move || {
            if !timed_out.get() {
                slint::quit_event_loop().expect("stop timed-out protected close test loop");
            }
        });
        slint::run_event_loop_until_quit().expect("run protected close test loop");
        poll_timer.stop();
        assert!(close_approved.get(), "dirty draft close did not complete");
        assert_eq!(phase.get(), 1);
        assert!(!harness.ui.get_composer_open());
        assert!(!harness.controller.compose_dirty.get());
        assert!(!harness.controller.window_close_pending.get());
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
        let (draft_subject, body_file_key, body_byte_count) = connection
            .query_row(
                "SELECT message.subject, content.body_file_key, content.body_byte_count
                   FROM local_drafts AS draft
                   JOIN messages AS message ON message.id = draft.message_id
                   JOIN message_content AS content ON content.message_id = draft.message_id
                  WHERE message.account_id = 51
                  ORDER BY draft.updated_at_ms DESC
                  LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .expect("read protected close draft");
        assert_eq!(draft_subject, "Protected close draft");
        assert_eq!(body_byte_count, 51);
        assert_eq!(
            fs::read_to_string(database.path.with_file_name("content").join(body_file_key))
                .expect("read protected private draft body"),
            "This body must be durable before the window closes."
        );
        drop(connection);

        let empty_database = TestDatabase::new("empty");
        let mut empty = ControllerHarness::start(&empty_database.path);
        empty.drive_until("empty mailbox", |ui| mailbox_ready(ui, 0));
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
        dirty.drive_until("repaired mailbox retry", |ui| mailbox_ready(ui, 0));
        assert!(dirty.ui.get_has_accounts());
        assert!(dirty.ui.get_mail_actions_enabled());
        dirty.shutdown();

        let outbox_database = TestDatabase::new("outbox");
        let (message_id, mime_path) = outbox_database.seed_failed_outbox();
        assert!(mime_path.is_file());
        let (core, core_events, runtime) =
            core::spawn(outbox_database.path.clone()).expect("start outbox controller core");
        let outbox_ui = AppWindow::new().expect("create outbox AppWindow");
        let controller_task =
            install(&outbox_ui, core, core_events).expect("install production outbox controller");

        let phase = Rc::new(Cell::new(0_u8));
        let completed = Rc::new(Cell::new(false));
        let poll_timer = Timer::default();
        let poll_ui = outbox_ui.as_weak();
        let poll_phase = phase.clone();
        let poll_completed = completed.clone();
        let poll_database = outbox_database.path.clone();
        let poll_mime = mime_path.clone();
        poll_timer.start(TimerMode::Repeated, Duration::from_millis(10), move || {
            let Some(ui) = poll_ui.upgrade() else {
                return;
            };
            match poll_phase.get() {
                0 if !ui.get_outbox_loading() && ui.get_outbox_count() == 1 => {
                    let item = ui
                        .get_outbox_items()
                        .row_data(0)
                        .expect("persistent outbox row");
                    assert_eq!(item.message_id.as_str(), message_id.to_string());
                    assert_eq!(item.account_label.as_str(), "Send account");
                    assert_eq!(item.recipients.as_str(), "recipient@example.test");
                    assert_eq!(item.status.as_str(), "Not sent");
                    assert!(item.attempt_detail.contains("credentials"));
                    assert!(!item.attempt_detail.contains("smtp_authentication_rejected"));
                    assert!(item.can_update_credential);

                    ui.invoke_open_outbox();
                    poll_phase.set(1);
                }
                1 if ui.get_outbox_open() && !ui.get_outbox_loading() => {
                    ui.invoke_repair_outbox_credential(message_id.to_string().into());
                    assert!(ui.get_outbox_open());
                    assert!(ui.get_account_status_open());
                    assert!(ui.get_account_credential_open());
                    assert_eq!(ui.get_managed_account_id().as_str(), "1");

                    ui.invoke_update_account_credential("1".into(), SharedString::default());
                    assert!(ui.get_account_credential_open());
                    assert!(!ui.get_account_operation_loading());
                    assert!(
                        ui.get_account_operation_error()
                            .contains("non-empty app password")
                    );
                    ui.set_account_credential_open(false);
                    ui.set_account_status_open(false);
                    ui.set_account_operation_error(SharedString::default());
                    ui.invoke_close_outbox();
                    assert!(!ui.get_outbox_open());
                    ui.invoke_open_outbox();
                    poll_phase.set(2);
                }
                2 if ui.get_outbox_open() && !ui.get_outbox_loading() => {
                    ui.invoke_release_failed_outbox(message_id.to_string().into());
                    poll_phase.set(3);
                }
                3 if !ui.get_outbox_action_loading() && ui.get_outbox_count() == 0 => {
                    let Ok(connection) = Connection::open(&poll_database) else {
                        return;
                    };
                    let Ok(state) = connection.query_row(
                        "SELECT
                             (SELECT count(*) FROM outbox),
                             (SELECT locked_artifact_generation
                                FROM local_drafts WHERE message_id = ?1),
                             (SELECT drafts_total FROM account_mailbox_stats WHERE account_id = 1),
                             (SELECT count(*) FROM file_gc)",
                        [message_id],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, Option<i64>>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                            ))
                        },
                    ) else {
                        return;
                    };
                    if state == (0, None, 1, 0) && !poll_mime.exists() {
                        assert_eq!(ui.get_status_text().as_str(), "Message returned to Drafts");
                        poll_completed.set(true);
                        slint::quit_event_loop().expect("stop outbox controller test loop");
                    }
                }
                _ => {}
            }
        });
        let timed_out = completed.clone();
        Timer::single_shot(Duration::from_secs(5), move || {
            if !timed_out.get() {
                slint::quit_event_loop().expect("stop timed-out outbox controller test loop");
            }
        });
        slint::run_event_loop_until_quit().expect("run outbox controller test loop");
        assert!(completed.get(), "persistent outbox UI flow timed out");
        controller_task.abort();
        runtime.shutdown().expect("stop outbox controller core");
    }
}
