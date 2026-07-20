use crate::AppWindow;
use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
use slint::{BackendSelector, CloseRequestResponse, ComponentHandle};

pub(crate) fn select_backend() -> Result<(), slint::PlatformError> {
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
        .select()
}

pub(crate) fn install_window_handlers(ui: &AppWindow) {
    ui.window().on_winit_window_event(|window, event| {
        if should_cancel_pointer_interaction(event)
            && let Err(error) =
                window.try_dispatch_event(slint::platform::WindowEvent::PointerExited)
        {
            eprintln!("Could not reset pointer state after window focus loss: {error}");
        }
        EventResult::Propagate
    });

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
        ui.on_window_exit_approved(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = ui.hide();
                let _ = slint::quit_event_loop();
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.window().on_close_requested(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.invoke_window_close();
            }
            CloseRequestResponse::KeepWindowShown
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
}

fn should_cancel_pointer_interaction(event: &winit::event::WindowEvent) -> bool {
    matches!(event, winit::event::WindowEvent::Focused(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_focus_loss_cancels_an_in_flight_pointer_interaction() {
        assert!(should_cancel_pointer_interaction(
            &winit::event::WindowEvent::Focused(false)
        ));
        assert!(!should_cancel_pointer_interaction(
            &winit::event::WindowEvent::Focused(true)
        ));
    }
}
