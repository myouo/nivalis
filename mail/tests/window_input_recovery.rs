use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, PhysicalSize};

slint::include_modules!();

#[test]
fn pointer_exit_cancels_a_stale_press_and_the_next_click_succeeds() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        renderer_name: Some("skia".into()),
        ..Default::default()
    })))
    .unwrap();

    let ui = AppWindow::new().unwrap();
    ui.window().set_size(PhysicalSize::new(1_200, 800));
    ui.show().unwrap();
    slint::platform::update_timers_and_animations();

    let dark_mode_button = LogicalPosition::new(1_038.0, 20.0);
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position: dark_mode_button,
        button: PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerExited);
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position: dark_mode_button,
        button: PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position: dark_mode_button,
        button: PointerEventButton::Left,
    });

    assert!(ui.get_dark_mode());
}
