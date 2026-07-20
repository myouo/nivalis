use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, PhysicalSize};

slint::include_modules!();

#[test]
fn long_unbroken_reader_content_renders_at_desktop_and_compact_widths() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        renderer_name: Some("skia".into()),
        ..Default::default()
    })))
    .unwrap();

    let ui = AppWindow::new().unwrap();
    let long_token = "x".repeat(4_096);
    let body =
        format!("正文第一行。\nhttps://example.test/{long_token}\n\nA final readable paragraph.");
    ui.set_selected_id("1".into());
    ui.set_selected_mail(MailDetail {
        id: "1".into(),
        sender: "Render regression".into(),
        email: "render@example.test".into(),
        initials: "RR".into(),
        subject: "Long body rendering remains constrained to the reader pane".into(),
        body: body.into(),
        body_truncated: false,
        date: "2026-07-20 13:00 UTC".into(),
        folder: "Inbox".into(),
        starred: false,
        has_attachment: false,
        avatar_color: slint::Color::from_rgb_u8(47, 93, 120),
    });

    for size in [PhysicalSize::new(1_200, 800), PhysicalSize::new(680, 560)] {
        ui.window().set_size(size);
        ui.set_detail_open(true);
        ui.show().unwrap();
        slint::platform::update_timers_and_animations();

        let snapshot = ui.window().take_snapshot().unwrap();
        assert_eq!(snapshot.width(), size.width);
        assert_eq!(snapshot.height(), size.height);
        ui.hide().unwrap();
    }
}
