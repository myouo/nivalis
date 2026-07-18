use crate::core::{CoreHandle, Event, EventReceiver, OperationId, SubmitError};
use crate::presentation::{
    apply_view, refresh_account, refresh_folder, refresh_selection, refresh_stats, show_snackbar,
    show_snackbar_after_event, update_mail_row,
};
use crate::store::MailStore;
use crate::{AccountItem, AppWindow, MailSummary};
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

pub(crate) fn install(
    ui: &AppWindow,
    core: CoreHandle,
    mut core_events: EventReceiver,
) -> Result<slint::JoinHandle<()>, slint::EventLoopError> {
    let store = Rc::new(RefCell::new(MailStore::demo()));
    let (initial_view, initial_accounts) = {
        let store = store.borrow();
        let view = store.view();
        let accounts = store.accounts_with_stats(&view.stats);
        (view, accounts)
    };
    let mail_model = Rc::new(VecModel::<MailSummary>::from(initial_view.rows));
    let account_model = Rc::new(VecModel::<AccountItem>::from(initial_accounts));
    let snackbar_timer = Rc::new(Timer::default());
    let search_timer = Rc::new(Timer::default());
    let active_sync = Rc::new(Cell::new(None));
    let next_operation_id = Rc::new(Cell::new(1_u64));
    ui.set_mails(ModelRc::from(mail_model.clone()));
    ui.set_accounts(ModelRc::from(account_model.clone()));
    {
        let store = store.borrow();
        refresh_selection(ui, &store);
        refresh_folder(ui, &store);
        refresh_account(ui, &store);
        refresh_stats(ui, &account_model, &initial_view.stats);
    }
    ui.set_initial_loading(false);
    ui.set_status_text("Updated just now".into());

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let mail_model = mail_model.clone();
        let account_model = account_model.clone();
        ui.on_select_mail(move |id| {
            store.borrow_mut().select(id);
            if let Some(ui) = ui_weak.upgrade() {
                let store = store.borrow();
                if store.active_folder() == "Unread" {
                    let view = store.view();
                    apply_view(&ui, &store, &mail_model, &account_model, view);
                } else {
                    update_mail_row(&mail_model, &store, id);
                    refresh_selection(&ui, &store);
                    refresh_stats(&ui, &account_model, &store.stats());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        ui.on_load_full_message(move |id| {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let store = store.borrow();
            if store.selected_id() == Some(id) {
                ui.set_selected_mail(store.selected_full());
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let mail_model = mail_model.clone();
        let account_model = account_model.clone();
        ui.on_filter_folder(move |folder| {
            store.borrow_mut().set_folder(folder.as_str());
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_detail_open(false);
                let store = store.borrow();
                let view = store.view();
                apply_view(&ui, &store, &mail_model, &account_model, view);
                refresh_folder(&ui, &store);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let mail_model = mail_model.clone();
        let account_model = account_model.clone();
        let search_timer = search_timer.clone();
        ui.on_query_mail(move |query| {
            let ui_weak = ui_weak.clone();
            let store = store.clone();
            let mail_model = mail_model.clone();
            let account_model = account_model.clone();
            search_timer.start(
                TimerMode::SingleShot,
                Duration::from_millis(180),
                move || {
                    store.borrow_mut().set_query(query.as_str());
                    if let Some(ui) = ui_weak.upgrade() {
                        let store = store.borrow();
                        let view = store.view();
                        apply_view(&ui, &store, &mail_model, &account_model, view);
                    }
                },
            );
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let mail_model = mail_model.clone();
        let account_model = account_model.clone();
        ui.on_toggle_star(move |id| {
            store.borrow_mut().toggle_star(id);
            if let Some(ui) = ui_weak.upgrade() {
                let store = store.borrow();
                if store.active_folder() == "Starred" {
                    let view = store.view();
                    apply_view(&ui, &store, &mail_model, &account_model, view);
                } else {
                    update_mail_row(&mail_model, &store, id);
                    if store.selected_id() == Some(id) {
                        refresh_selection(&ui, &store);
                    }
                    refresh_stats(&ui, &account_model, &store.stats());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let mail_model = mail_model.clone();
        let account_model = account_model.clone();
        let snackbar_timer = snackbar_timer.clone();
        ui.on_archive(move |id| {
            store.borrow_mut().archive(id);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_status_text("Moved to archive".into());
                show_snackbar(&ui, "Message moved to archive", false, &snackbar_timer);
                let store = store.borrow();
                let view = store.view();
                apply_view(&ui, &store, &mail_model, &account_model, view);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let mail_model = mail_model.clone();
        let account_model = account_model.clone();
        let snackbar_timer = snackbar_timer.clone();
        ui.on_delete_mail(move |id| {
            let can_undo = store.borrow_mut().delete(id);
            if let Some(ui) = ui_weak.upgrade() {
                let message = if can_undo {
                    "Message moved to trash"
                } else {
                    "Message permanently deleted"
                };
                ui.set_status_text(message.into());
                show_snackbar(&ui, message, can_undo, &snackbar_timer);
                let store = store.borrow();
                let view = store.view();
                apply_view(&ui, &store, &mail_model, &account_model, view);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let mail_model = mail_model.clone();
        let account_model = account_model.clone();
        let snackbar_timer = snackbar_timer.clone();
        ui.on_undo_delete(move || {
            let restored_id = store.borrow_mut().undo_delete();
            if let Some(ui) = ui_weak.upgrade() {
                if restored_id.is_some() {
                    let store = store.borrow();
                    let view = store.view();
                    apply_view(&ui, &store, &mail_model, &account_model, view);
                    ui.set_status_text("Message restored".into());
                    show_snackbar_after_event(&ui, "Message restored", snackbar_timer.clone());
                } else {
                    ui.set_status_text("Message could not be restored".into());
                    show_snackbar_after_event(&ui, "Nothing to restore", snackbar_timer.clone());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let mail_model = mail_model.clone();
        let account_model = account_model.clone();
        ui.on_mark_unread(move |id| {
            store.borrow_mut().mark_unread(id);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_status_text("Marked as unread".into());
                let store = store.borrow();
                if store.active_folder() == "Unread" {
                    let view = store.view();
                    apply_view(&ui, &store, &mail_model, &account_model, view);
                } else {
                    update_mail_row(&mail_model, &store, id);
                    refresh_stats(&ui, &account_model, &store.stats());
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let mail_model = mail_model.clone();
        let account_model = account_model.clone();
        let snackbar_timer = snackbar_timer.clone();
        ui.on_send_message(move |recipient, subject, body| {
            if recipient.trim().is_empty() || subject.trim().is_empty() {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_status_text("Recipient and subject are required".into());
                    ui.set_composer_error("Add a recipient and subject before sending.".into());
                }
                return false;
            }

            store
                .borrow_mut()
                .send(recipient.as_str(), subject.as_str(), body.as_str());
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_composer_open(false);
                ui.set_composer_error("".into());
                ui.set_status_text("Message sent".into());
                show_snackbar(&ui, "Message sent", false, &snackbar_timer);
                let store = store.borrow();
                let view = store.view();
                apply_view(&ui, &store, &mail_model, &account_model, view);
            }
            true
        });
    }

    {
        let ui_weak = ui.as_weak();
        let store = store.clone();
        let mail_model = mail_model.clone();
        let account_model = account_model.clone();
        let snackbar_timer = snackbar_timer.clone();
        ui.on_switch_account(move |account_id| {
            if !store.borrow_mut().set_account(account_id) {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_status_text("Account is no longer available".into());
                    show_snackbar(
                        &ui,
                        "Account is no longer available",
                        false,
                        &snackbar_timer,
                    );
                }
                return;
            }

            if let Some(ui) = ui_weak.upgrade() {
                ui.set_detail_open(false);
                let store = store.borrow();
                let view = store.view();
                apply_view(&ui, &store, &mail_model, &account_model, view);
                refresh_account(&ui, &store);
                ui.set_status_text(format!("Showing {}", store.active_account_name()).into());
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let active_sync = active_sync.clone();
        let next_operation_id = next_operation_id.clone();
        let snackbar_timer = snackbar_timer.clone();
        ui.on_sync(move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            if ui.get_syncing() {
                return;
            }

            let operation_id = OperationId::new(next_operation_id.get());
            next_operation_id.set(next_operation_id.get().wrapping_add(1).max(1));
            ui.set_syncing(true);
            ui.set_status_text("Syncing mail...".into());

            match core.try_send_sync(operation_id) {
                Ok(()) => active_sync.set(Some(operation_id)),
                Err(error) => {
                    active_sync.set(None);
                    ui.set_syncing(false);
                    let (status, message) = match error {
                        SubmitError::Busy => (
                            "Sync queue is busy",
                            "Sync is already queued. Try again shortly.",
                        ),
                        SubmitError::Closed => (
                            "Sync service unavailable",
                            "Sync service stopped. Restart Nivalis Mail.",
                        ),
                    };
                    ui.set_status_text(status.into());
                    show_snackbar(&ui, message, false, &snackbar_timer);
                }
            }
        });
    }

    let ui_weak = ui.as_weak();
    slint::spawn_local(async move {
        while let Some(event) = core_events.recv().await {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            match event {
                Event::SyncFinished { operation_id } if active_sync.get() == Some(operation_id) => {
                    active_sync.set(None);
                    ui.set_syncing(false);
                    ui.set_status_text("Synced just now".into());
                    show_snackbar(&ui, "Mailbox is up to date", false, &snackbar_timer);
                }
                Event::SyncFinished { .. } => {}
                Event::MailboxLoaded(_)
                | Event::MailboxLoadRejected { .. }
                | Event::MessageLoaded(_)
                | Event::MessageLoadRejected { .. } => {}
            }
        }

        if active_sync.take().is_some()
            && let Some(ui) = ui_weak.upgrade()
        {
            ui.set_syncing(false);
            ui.set_status_text("Sync service unavailable".into());
            show_snackbar(
                &ui,
                "Sync service stopped. Restart Nivalis Mail.",
                false,
                &snackbar_timer,
            );
        }
    })
}
