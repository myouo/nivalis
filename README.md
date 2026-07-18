# Nivalis Mail

A task-first desktop mail client prototype built with Rust, Slint, and the Skia renderer. Its compact visual language is independently designed from Fluent-style desktop patterns: flat surfaces, thin outlines, stable navigation, and progressive disclosure without copying third-party code or assets.

## Run

```bash
cargo run
```

The app explicitly selects Winit with Skia before creating its window. The first build is larger because Cargo needs to prepare Skia.

The standard release profile is tuned for the measured performance/working-set Pareto point:

```bash
cargo build --release                 # opt-level=s, recommended
cargo build --profile performance     # opt-level=3, maximum active throughput
cargo build --profile release-size    # opt-level=z, minimum binary/RSS
```

The default low-memory profile uses Skia CPU rasterization with partial rendering and a native softbuffer surface. GPU-accelerated Skia remains available for animation-heavy or unusually large workloads:

```bash
NIVALIS_RENDERER=skia cargo run --release
```

## Experience

- Frameless 40px title bar, 248px collapsible sidebar, 54px rail, and adaptive single-pane reading below 760px
- Inbox-first three-pane workspace with account-aware search and progressive folder filters
- Three demo accounts plus an aggregated `All inboxes` view
- Read, star, archive, mark-unread, compose, send, and reply flows
- Confirmed trash actions, permanent deletion, and a timed undo snackbar
- Static loading rows, account error, empty/search-empty, sync progress, disabled, and form error states
- Light, dark, high-contrast, and reduced-motion display preferences
- A 110KB Material Symbols subset, system Noto Sans typography, 48px interaction targets, focus rings, semantic accessibility labels, and live-region feedback

## Keyboard

- `Tab` and `Shift+Tab` move through controls.
- `Enter` or `Space` activates focused custom controls.
- `Ctrl+N` opens the composer.
- `Escape` closes the active menu, dialog, composer, or compact reading view.

## Structure

- `ui/theme.slint` contains color, typography, contrast, and motion tokens.
- `ui/components.slint` contains reusable compact desktop controls and state layers.
- `ui/app.slint` contains the adaptive mailbox, reader, dialogs, sheets, and status surfaces.
- `src/store.rs` contains the local multi-account mail model and tested state transitions.

The embedded icon subset is generated from Material Symbols Rounded and retains its upstream license in `assets/licenses`. Text uses the system's Noto Sans installation, with normal platform fallback behavior.

## Resource strategy

- Slint is built with `default-features = false` and only the Winit, Skia, accessibility, and compatibility features required by this app.
- The visible model is capped at 50 lightweight summaries and stays virtualized through `ListView`; only the selected message materializes its full body.
- A single Store pass produces the bounded page, total results, folder counters, and all per-account unread counts; account rows update only when their count changes.
- Stable mail text is stored as `SharedString`, so summary/detail projection clones a shared handle rather than copying sender, subject, preview, or body data.
- Search uses an allocation-free ASCII case-insensitive matcher and a restartable 180ms debounce instead of rebuilding the model on every keystroke.
- Dialogs, menus, settings, and the composer are conditionally instantiated and no decorative animation or periodic UI timer runs while idle.
- Sync and snackbar feedback reuse one restartable timer each, so bursty actions cannot accumulate delayed closures.
- Row-only mutations update one existing row; membership changes rebuild one strict 50-row snapshot, preventing 49/51-row pagination drift.
- Message previews are capped at 280 Unicode scalar values, and reader shaping is capped at 16,384 values until the user explicitly loads an unusually large body.
- Local cache content renders immediately instead of constructing and discarding an artificial 650ms skeleton state.
- Production releases exclude the environment-driven stress harness. Benchmark it explicitly with `cargo build --release --features bench-harness`.
- Release profiles use full LTO, one codegen unit, stripped symbols, and abort-on-panic. The recommended `s` profile retained `3`-level stress throughput within 2.5% while reducing the measured stress RSS by about 2MiB.
- A production mailbox should back the same 50-row page with SQLite/FTS and load bodies and attachments from disk on demand.

The verified Linux release executable is 18.0MB. Across three fresh X11 runs at 1200x900, the recommended release profile's worst idle sample was 35.5MiB RSS / 21.2MiB PSS / 18.0MiB USS. Three native Wayland runs stayed below 41.6MiB RSS / 22.4MiB PSS / 17.6MiB USS. See `memory-report.md` for the measurement contract, stress results, and reproduction command.

This is an interaction-complete local prototype. Production email support still requires an IMAP/JMAP or provider API adapter, SMTP submission, encrypted credential storage, a durable local database, and safe HTML mail rendering.
