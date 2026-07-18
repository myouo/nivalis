# Nivalis Mail

A task-first desktop mail client prototype built with Rust, Slint, and the Skia renderer. Its compact visual language is independently designed from Fluent-style desktop patterns: flat surfaces, thin outlines, stable navigation, and progressive disclosure without copying third-party code or assets.

## Technology

The confirmed application stack is Rust 2024 with Slint 1.17.1, Winit, and the Skia renderer. The production architecture keeps the native Slint UI on the main thread, runs network work on one Tokio current-thread runtime, and serializes SQLite access through a dedicated `rusqlite` actor. Mail transport uses IMAP and SMTP, with JMAP as an optional provider backend; MIME parsing, OAuth2, Rustls, and the `keyring` crate complete the protocol and credential boundary. Nivalis does not embed a WebView.

These production services are architectural commitments and are being integrated incrementally; the current repository remains an interaction-complete local prototype. See [`docs/architecture.md`](docs/architecture.md) for ownership boundaries, backpressure rules, dependency features, and memory budgets.

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

- `ui/app.slint` is the single Slint build entry and owns the stable Rust-facing window API, responsive state, keyboard routing, and conditional composition.
- `ui/models.slint` and `ui/theme.slint` define shared UI data types and visual tokens without depending on feature views.
- `ui/components.slint` is a compatibility facade over the primitives, controls, inputs, navigation, and feedback modules in `ui/components/`.
- `ui/shared.slint`, `ui/actions.slint`, and `ui/shell.slint` contain cross-feature presentation elements, reader/compose actions, the title bar, and adaptive navigation.
- `ui/mailbox.slint`, `ui/reader.slint`, `ui/composer.slint`, and `ui/states.slint` isolate the main task surfaces and reusable empty state.
- `ui/overlays.slint` contains menus, settings, confirmation, and snackbar surfaces; `ui/app.slint` keeps their `if` boundaries so hidden overlays are not instantiated.
- `src/main.rs` selects the process modules and starts the Slint event loop.
- `src/platform.rs` owns renderer selection and native window integration.
- `src/controller.rs` binds user intents to the current application services.
- `src/presentation.rs` projects bounded Store snapshots into Slint models.
- `src/benchmark.rs` contains the opt-in memory stress harness.
- `src/store/` is the repository facade; `memory.rs` is the replaceable prototype backend.

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

The verified Linux release executable is 18.0MB. Across three fresh X11 runs at 1200x900 after UI modularization, the recommended release profile's worst idle sample was 35.7MiB RSS / 21.8MiB PSS / 18.4MiB USS. Three native Wayland runs stayed below 41.6MiB RSS / 22.4MiB PSS / 17.6MiB USS. See `memory-report.md` for the measurement contract, stress results, and reproduction command.

This is an interaction-complete local prototype. Production email support is being added within the fixed architecture above: IMAP/SMTP by default, optional JMAP, encrypted credential storage, a durable local database, and bounded native rendering for mail content.

## License

Nivalis Mail is distributed under the **GNU General Public License, version 3 only** (`GPL-3.0-only`). Distributions and modified versions must comply with GPLv3, including its corresponding-source and license-notice requirements. See [`LICENSE`](LICENSE) for the complete license text.
