use crate::core::{CoreHandle, Event, EventReceiver};
use crate::presentation::{
    apply_view, refresh_account, refresh_folder, refresh_selection, refresh_stats, show_snackbar,
    show_snackbar_after_event, update_mail_row,
};
use crate::store::MailStore;
use crate::{AccountItem, AppWindow, MailSummary};
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
use std::{cell::RefCell, rc::Rc, time::Duration};

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
    ui.set_status_text("Local preview".into());

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
                ui.set_status_text("Local sample moved to archive".into());
                show_snackbar(
                    &ui,
                    "Local sample message moved to archive",
                    false,
                    &snackbar_timer,
                );
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
                    "Local sample message moved to trash"
                } else {
                    "Local sample message removed"
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
                    ui.set_status_text("Local sample message restored".into());
                    show_snackbar_after_event(
                        &ui,
                        "Local sample message restored",
                        snackbar_timer.clone(),
                    );
                } else {
                    ui.set_status_text("Local sample could not be restored".into());
                    show_snackbar_after_event(
                        &ui,
                        "No local sample message to restore",
                        snackbar_timer.clone(),
                    );
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
                ui.set_status_text("Local sample marked unread".into());
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
                ui.set_status_text(
                    format!("Showing {} local sample", store.active_account_name()).into(),
                );
            }
        });
    }

    let ui_weak = ui.as_weak();
    slint::spawn_local(async move {
        while let Some(event) = core_events.recv().await {
            let Some(_ui) = ui_weak.upgrade() else {
                return;
            };
            match event {
                Event::AccountsLoaded(_)
                | Event::AccountsLoadRejected { .. }
                | Event::MailboxLoaded(_)
                | Event::MailboxLoadRejected { .. }
                | Event::MessageLoaded(_)
                | Event::MessageLoadRejected { .. }
                | Event::MutationFinished(_)
                | Event::MutationRejected { .. } => {}
            }
        }
        drop(core);
    })
}
