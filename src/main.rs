mod store;

use slint::winit_030::WinitWindowAccessor;
use slint::{
    BackendSelector, ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel,
};
#[cfg(feature = "bench-harness")]
use std::cell::Cell;
#[cfg(feature = "bench-harness")]
use std::time::Instant;
use std::{cell::RefCell, rc::Rc, time::Duration};
use store::{MailStats, MailStore, MailView};

slint::include_modules!();

fn refresh_selection(ui: &AppWindow, store: &MailStore) {
    ui.set_selected_mail(store.selected());
    ui.set_selected_id(store.selected_id().unwrap_or(-1));
}

fn refresh_folder(ui: &AppWindow, store: &MailStore) {
    ui.set_active_folder(SharedString::from(store.active_folder()));
}

fn refresh_account(ui: &AppWindow, store: &MailStore) {
    ui.set_active_account_id(store.active_account_id());
    ui.set_active_account_name(store.active_account_name().into());
    ui.set_active_account_detail(store.active_account_detail().into());
    ui.set_active_account_initials(store.active_account_initials().into());
    ui.set_active_account_color(store.active_account_color());
    ui.set_active_account_error(store.active_account_error());
}

fn refresh_stats(ui: &AppWindow, account_model: &VecModel<AccountItem>, stats: &MailStats) {
    ui.set_message_total(stats.message_total);
    ui.set_inbox_count(stats.inbox_count);
    ui.set_starred_count(stats.starred_count);
    ui.set_draft_count(stats.draft_count);

    for (index, unread_count) in stats.account_unread.into_iter().enumerate() {
        let Some(mut account) = account_model.row_data(index) else {
            continue;
        };
        if account.unread_count != unread_count {
            account.unread_count = unread_count;
            account_model.set_row_data(index, account);
        }
    }
}

fn apply_view(
    ui: &AppWindow,
    store: &MailStore,
    mail_model: &VecModel<MailSummary>,
    account_model: &VecModel<AccountItem>,
    view: MailView,
) {
    mail_model.set_vec(view.rows);
    refresh_selection(ui, store);
    refresh_stats(ui, account_model, &view.stats);
}

fn show_snackbar(ui: &AppWindow, message: &str, can_undo: bool, timer: &Timer) {
    let message = SharedString::from(message);
    ui.set_snackbar_text(message.clone());
    ui.set_snackbar_can_undo(can_undo);
    ui.set_snackbar_visible(true);

    let expected_message = message;
    let ui_weak = ui.as_weak();
    timer.start(TimerMode::SingleShot, Duration::from_secs(5), move || {
        if let Some(ui) = ui_weak.upgrade()
            && ui.get_snackbar_text() == expected_message
        {
            ui.set_snackbar_visible(false);
        }
    });
}

fn show_snackbar_after_event(ui: &AppWindow, message: &'static str, timer: Rc<Timer>) {
    let ui_weak = ui.as_weak();
    Timer::single_shot(Duration::ZERO, move || {
        if let Some(ui) = ui_weak.upgrade() {
            show_snackbar(&ui, message, false, &timer);
        }
    });
}

fn update_mail_row(model: &VecModel<MailSummary>, store: &MailStore, id: i32) {
    let Some(old_index) = (0..model.row_count())
        .find(|index| model.row_data(*index).is_some_and(|mail| mail.id == id))
    else {
        return;
    };
    let Some((new_index, mail)) = store.visible_mail(id) else {
        return;
    };
    debug_assert_eq!(old_index, new_index, "row-only mutation changed page order");
    if old_index == new_index {
        model.set_row_data(old_index, mail);
    }
}

#[cfg(feature = "bench-harness")]
fn install_memory_stress(ui: &AppWindow) -> Option<Rc<Timer>> {
    let steps = std::env::var("NIVALIS_STRESS_STEPS")
        .ok()?
        .parse::<usize>()
        .ok()?
        .max(1);
    let delay = std::env::var("NIVALIS_STRESS_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(5_000);
    let interval = std::env::var("NIVALIS_STRESS_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(2)
        .max(1);

    let timer = Rc::new(Timer::default());
    let timer_weak = Rc::downgrade(&timer);
    let ui_weak = ui.as_weak();
    let step = Rc::new(Cell::new(0usize));

    Timer::single_shot(Duration::from_millis(delay), move || {
        let Some(timer) = timer_weak.upgrade() else {
            return;
        };
        let started = Instant::now();
        let timer_for_callback = Rc::downgrade(&timer);
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(interval),
            move || {
                let Some(ui) = ui_weak.upgrade() else {
                    return;
                };
                let current = step.get();
                if current >= steps {
                    ui.set_settings_open(false);
                    ui.set_account_menu_open(false);
                    ui.set_composer_open(false);
                    ui.set_compose_to("".into());
                    ui.set_compose_subject("".into());
                    ui.set_compose_body("".into());
                    ui.set_search_query("".into());
                    ui.invoke_query_mail("".into());
                    ui.set_status_text("Memory stress complete".into());
                    eprintln!(
                        "NIVALIS_STRESS_RESULT steps={steps} elapsed_ms={}",
                        started.elapsed().as_millis()
                    );
                    if let Some(timer) = timer_for_callback.upgrade() {
                        timer.stop();
                    }
                    if std::env::var("NIVALIS_STRESS_EXIT").as_deref() == Ok("1") {
                        let _ = ui.hide();
                        let _ = slint::quit_event_loop();
                    }
                    return;
                }

                const IDS: [i32; 10] = [1, 2, 3, 4, 5, 8, 9, 10, 11, 12];
                let id = IDS[current % IDS.len()];
                match current % 8 {
                    0 => ui.invoke_select_mail(id),
                    1 => ui.invoke_toggle_star(id),
                    2 => {
                        ui.set_account_menu_open(false);
                        ui.set_settings_open(true);
                    }
                    3 => {
                        ui.set_settings_open(false);
                        ui.set_account_menu_open(true);
                    }
                    4 => {
                        ui.set_account_menu_open(false);
                        ui.set_composer_open(true);
                        ui.set_compose_to("stress@example.com".into());
                        ui.set_compose_subject("Bounded interaction stress".into());
                        if current == 4 {
                            ui.set_compose_body("x".repeat(64 * 1024).into());
                        }
                    }
                    5 => {
                        ui.set_composer_open(false);
                        ui.set_compose_to("".into());
                        ui.set_compose_subject("".into());
                        ui.set_compose_body("".into());
                    }
                    6 => ui.invoke_sync(),
                    _ => {
                        let query = if current % 16 == 7 { "maya" } else { "" };
                        ui.set_search_query(query.into());
                        ui.invoke_query_mail(query.into());
                    }
                }
                step.set(current + 1);
            },
        );
    });

    Some(timer)
}

#[cfg(feature = "bench-harness")]
fn install_maximize_stress(ui: &AppWindow) {
    if std::env::var("NIVALIS_MAXIMIZE_STRESS").as_deref() != Ok("1") {
        return;
    }

    let delay = std::env::var("NIVALIS_MAXIMIZE_STRESS_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(5_000);
    let duration = std::env::var("NIVALIS_MAXIMIZE_STRESS_DURATION_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(5_000);
    let ui_weak = ui.as_weak();

    Timer::single_shot(Duration::from_millis(delay), move || {
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        ui.window().set_maximized(true);

        let ui_weak = ui.as_weak();
        Timer::single_shot(Duration::from_millis(duration), move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.window().set_maximized(false);
            }
        });
    });
}

fn main() -> Result<(), slint::PlatformError> {
    let renderer_name = match std::env::var("NIVALIS_RENDERER").as_deref() {
        Ok("skia") => "skia",
        Ok("skia-software") | Err(_) => "skia-software",
        Ok(other) => {
            eprintln!("Unsupported NIVALIS_RENDERER={other}; using skia-software");
            "skia-software"
        }
    };

    BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name(renderer_name.into())
        .select()?;

    let ui = AppWindow::new()?;

    {
        let ui_weak = ui.as_weak();
        ui.on_window_minimize(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.window().set_minimized(true);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_window_maximize(move |maximized| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.window().set_maximized(maximized);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_window_close(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = ui.hide();
                let _ = slint::quit_event_loop();
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_window_drag(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.window().with_winit_window(|window| {
                    let _ = window.drag_window();
                });
            }
        });
    }

    let store = Rc::new(RefCell::new(MailStore::demo()));
    let (initial_view, initial_accounts) = {
        let store = store.borrow();
        let view = store.view();
        let accounts = store.accounts_with_stats(&view.stats);
        (view, accounts)
    };
    let mail_model = Rc::new(VecModel::from(initial_view.rows));
    let account_model = Rc::new(VecModel::from(initial_accounts));
    let snackbar_timer = Rc::new(Timer::default());
    let search_timer = Rc::new(Timer::default());
    let sync_timer = Rc::new(Timer::default());
    ui.set_mails(ModelRc::from(mail_model.clone()));
    ui.set_accounts(ModelRc::from(account_model.clone()));
    {
        let store = store.borrow();
        refresh_selection(&ui, &store);
        refresh_folder(&ui, &store);
        refresh_account(&ui, &store);
        refresh_stats(&ui, &account_model, &initial_view.stats);
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
        let sync_timer = sync_timer.clone();
        let snackbar_timer = snackbar_timer.clone();
        ui.on_sync(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if ui.get_syncing() {
                    return;
                }
                ui.set_syncing(true);
                ui.set_status_text("Syncing mail...".into());
            }

            let ui_weak = ui_weak.clone();
            let snackbar_timer = snackbar_timer.clone();
            sync_timer.start(
                TimerMode::SingleShot,
                Duration::from_millis(900),
                move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_syncing(false);
                        ui.set_status_text("Synced just now".into());
                        show_snackbar(&ui, "Mailbox is up to date", false, &snackbar_timer);
                    }
                },
            );
        });
    }

    #[cfg(feature = "bench-harness")]
    let _memory_stress_timer = install_memory_stress(&ui);
    #[cfg(feature = "bench-harness")]
    install_maximize_stress(&ui);
    ui.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_update_does_not_reset_the_visible_model() {
        let mut store = MailStore::demo();
        let model = VecModel::from(store.filtered());
        let original_count = model.row_count();

        store.toggle_star(2);
        update_mail_row(&model, &store, 2);

        assert_eq!(model.row_count(), original_count);
        let updated = (0..model.row_count())
            .filter_map(|index| model.row_data(index))
            .find(|mail| mail.id == 2)
            .expect("updated mail should remain in the model");
        assert!(updated.starred);
    }

    #[test]
    fn page_rebuild_stays_bounded_after_membership_changes() {
        let mut store = MailStore::demo();
        store.set_folder("Sent");
        let mut newest_id = 0;
        for index in 0..75 {
            newest_id = store.send("to@example.com", &format!("Message {index}"), "Body");
        }
        let model = VecModel::from(store.view().rows);
        assert_eq!(model.row_count(), 50);

        store.delete(newest_id);
        model.set_vec(store.view().rows);

        assert_eq!(model.row_count(), 50);
        assert!(
            (0..model.row_count())
                .filter_map(|index| model.row_data(index))
                .all(|mail| mail.id != newest_id)
        );
    }
}
