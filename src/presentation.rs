use crate::store::{MailStats, MailStore, MailView};
use crate::ui_identity::{AccountKey, EntityKey};
use crate::{AccountItem, AppWindow, MailSummary};
use slint::{ComponentHandle, Model, SharedString, Timer, TimerMode, VecModel};
use std::{rc::Rc, time::Duration};

pub(crate) fn refresh_selection(ui: &AppWindow, store: &MailStore) {
    ui.set_selected_mail(store.selected());
    ui.set_selected_id(store.selected_id().map(demo_entity_key).unwrap_or_default());
}

pub(crate) fn refresh_folder(ui: &AppWindow, store: &MailStore) {
    ui.set_active_folder(SharedString::from(store.active_folder()));
}

pub(crate) fn refresh_account(ui: &AppWindow, store: &MailStore) {
    ui.set_active_account_id(demo_account_key(store.active_account_id()));
    ui.set_active_account_name(store.active_account_name().into());
    ui.set_active_account_detail(store.active_account_detail().into());
    ui.set_active_account_initials(store.active_account_initials().into());
    ui.set_active_account_color(store.active_account_color());
    ui.set_active_account_error(store.active_account_error());
}

pub(crate) fn refresh_stats(
    ui: &AppWindow,
    account_model: &VecModel<AccountItem>,
    stats: &MailStats,
) {
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

pub(crate) fn apply_view(
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

pub(crate) fn show_snackbar(ui: &AppWindow, message: &str, can_undo: bool, timer: &Timer) {
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

pub(crate) fn show_snackbar_after_event(ui: &AppWindow, message: &'static str, timer: Rc<Timer>) {
    let ui_weak = ui.as_weak();
    Timer::single_shot(Duration::ZERO, move || {
        if let Some(ui) = ui_weak.upgrade() {
            show_snackbar(&ui, message, false, &timer);
        }
    });
}

pub(crate) fn update_mail_row(model: &VecModel<MailSummary>, store: &MailStore, id: i32) {
    let key = demo_entity_key(id);
    let Some(old_index) = (0..model.row_count())
        .find(|index| model.row_data(*index).is_some_and(|mail| mail.id == key))
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

fn demo_entity_key(id: i32) -> SharedString {
    EntityKey::new(i64::from(id))
        .expect("demo message IDs must be positive")
        .encode()
}

fn demo_account_key(id: i32) -> SharedString {
    AccountKey::from_scope_id(i64::from(id))
        .expect("demo account IDs must be zero or positive")
        .encode()
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
            .find(|mail| mail.id == demo_entity_key(2))
            .expect("updated mail should remain in the model");
        assert!(updated.starred);
    }

    #[test]
    fn page_rebuild_stays_bounded_after_membership_changes() {
        let mut store = MailStore::demo();
        store.set_folder("Sent");
        let mut newest_id = 0;
        for index in 0..75 {
            newest_id =
                store.insert_test_sent_mail("to@example.com", &format!("Message {index}"), "Body");
        }
        let model = VecModel::from(store.view().rows);
        assert_eq!(model.row_count(), 50);

        store.delete(newest_id);
        model.set_vec(store.view().rows);

        assert_eq!(model.row_count(), 50);
        assert!(
            (0..model.row_count())
                .filter_map(|index| model.row_data(index))
                .all(|mail| mail.id != demo_entity_key(newest_id))
        );
    }
}
