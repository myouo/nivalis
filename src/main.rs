#[cfg(feature = "bench-harness")]
mod benchmark;
mod controller;
mod platform;
mod presentation;
mod store;

use slint::ComponentHandle;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    platform::select_backend()?;

    let ui = AppWindow::new()?;
    platform::install_window_handlers(&ui);
    controller::install(&ui);

    #[cfg(feature = "bench-harness")]
    let _memory_stress_timer = benchmark::install_memory_stress(&ui);
    #[cfg(feature = "bench-harness")]
    benchmark::install_maximize_stress(&ui);

    ui.run()
}
