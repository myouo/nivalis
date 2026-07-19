#[cfg(feature = "bench-harness")]
mod benchmark;
#[allow(dead_code)]
mod content;
mod controller;
mod core;
#[allow(dead_code)]
mod credentials;
#[allow(dead_code)]
mod network;
mod platform;
mod presentation;
mod store;
mod ui_identity;

use slint::ComponentHandle;

slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    platform::select_backend()?;

    let ui = AppWindow::new()?;
    platform::install_window_handlers(&ui);
    let database_path = store::database_path()?;
    #[cfg(feature = "bench-harness")]
    let content_path = database_path.with_file_name("content");
    #[cfg(not(feature = "bench-harness"))]
    let (core, core_events, core_runtime) = core::spawn(database_path)?;
    #[cfg(feature = "bench-harness")]
    let (core, core_events, core_runtime, benchmark_database) =
        core::spawn_with_database(database_path)?;
    let _core_event_task = controller::install(&ui, core, core_events)?;

    #[cfg(feature = "bench-harness")]
    let _memory_stress_timer =
        benchmark::install_memory_stress(&ui, benchmark_database, content_path);
    #[cfg(feature = "bench-harness")]
    benchmark::install_maximize_stress(&ui);

    let ui_result = ui.run();
    let core_result = core_runtime.shutdown();

    ui_result?;
    core_result?;
    Ok(())
}
