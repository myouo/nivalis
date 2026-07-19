pub(crate) mod sqlite;

use crate::AppWindow;
use slint::{ComponentHandle, SharedString, Timer, TimerMode};
use std::time::Duration;

pub(crate) fn show_snackbar(ui: &AppWindow, message: &str, can_undo: bool, timer: &Timer) {
    show_snackbar_for(ui, message, can_undo, timer, Duration::from_secs(5));
}

pub(crate) fn show_snackbar_for(
    ui: &AppWindow,
    message: &str,
    can_undo: bool,
    timer: &Timer,
    duration: Duration,
) {
    let message = SharedString::from(message);
    ui.set_snackbar_text(message.clone());
    ui.set_snackbar_can_undo(can_undo);
    ui.set_snackbar_visible(true);

    let expected_message = message;
    let ui_weak = ui.as_weak();
    timer.start(TimerMode::SingleShot, duration, move || {
        if let Some(ui) = ui_weak.upgrade()
            && ui.get_snackbar_text() == expected_message
        {
            ui.set_snackbar_visible(false);
            ui.set_snackbar_can_undo(false);
        }
    });
}
