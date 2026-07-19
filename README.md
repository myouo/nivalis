# Nivalis Mail

A task-first desktop mail client prototype built with Rust, Slint, and the Skia renderer. Its compact visual language is independently designed from Fluent-style desktop patterns: flat surfaces, thin outlines, stable navigation, and progressive disclosure without copying third-party code or assets.

## Technology

The confirmed application stack is Rust 2024 with Slint 1.17.1, Winit, and the Skia renderer. The production architecture keeps the native Slint UI on the main thread, runs network work on one Tokio current-thread runtime, and serializes SQLite access through a dedicated `rusqlite` actor. Mail transport uses IMAP and SMTP, with JMAP as an optional provider backend; MIME parsing, OAuth2, Rustls, and the `keyring` crate complete the protocol and credential boundary. Nivalis does not embed a WebView.

The bounded Tokio core, schema-v8 persistence layer, keyset mailbox projections, and single-connection SQLite actor are now active. Local flags, Archive, Trash, Trash undo, and permanent deletion run as immediate transactions that compact the latest desired remote state with bounded undo, tombstones, deferred file cleanup, and persistent mailbox-statistic deltas in the same commit. Schema v8 stores mailbox-scoped IMAP locators, account-scoped JMAP state, a bounded desired-state journal, versioned lease metadata, and placement-rebase capacity reservations. The actor now exposes bounded claim snapshots and fully fenced report transactions for confirmation, already-satisfied state, progress renewal, retry, reconciliation, and blocking. Reports preserve IMAP/JMAP checkpoints, merge newer desired versions, roll back atomically, bypass UI reply backpressure, and drain after receiver cancellation or shutdown. Provider execution is deliberately not connected yet. SQLite replies enter the core through a bounded asynchronous channel; mailbox pages and selected-message details coalesce by request generation, while mutation results remain ordered control events and accepted writes are drained during shutdown. The controller still uses the in-memory repository until provider execution, synchronization merge/reconciliation, presentation/error mapping, and FTS reach parity, so the visible app remains an interaction-complete local prototype with one consistent UI data source. See [`docs/architecture.md`](docs/architecture.md) for ownership boundaries, backpressure rules, dependency features, implementation status, and memory budgets.

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
- `src/core/` owns the bounded command/event protocol and single-thread Tokio runtime.
- `src/presentation.rs` projects bounded Store snapshots into Slint models.
- `src/benchmark.rs` contains the opt-in memory stress harness.
- `src/store/` is the repository facade; `memory.rs` remains the controller's active prototype backend while `sqlite/` contains the active schema, projections, transactional mutations, migrations, and dedicated database actor. Controller integration remains staged.

The embedded icon subset is generated from Material Symbols Rounded and retains its upstream license in `assets/licenses`. Text uses the system's Noto Sans installation, with normal platform fallback behavior.

## Resource strategy

- Slint is built with `default-features = false` and only the Winit, Skia, accessibility, and compatibility features required by this app.
- The visible model is capped at 50 lightweight summaries and stays virtualized through `ListView`; only the selected message materializes its full body.
- A single Store pass produces the bounded page, total results, folder counters, and all per-account unread counts; account rows update only when their count changes.
- Stable mail text is stored as `SharedString`, so summary/detail projection clones a shared handle rather than copying sender, subject, preview, or body data.
- Search uses an allocation-free ASCII case-insensitive matcher and a restartable 180ms debounce instead of rebuilding the model on every keystroke.
- Dialogs, menus, settings, and the composer are conditionally instantiated and no decorative animation or periodic UI timer runs while idle.
- Sync crosses bounded 64-command and 128-event channels without blocking the UI; snackbar feedback alone uses a restartable UI timer.
- Full mailbox and reader projections never accumulate in the 128-slot control queue: it carries lightweight notifications while independent latest-value slots retain at most one 50-row page and one 64KiB detail.
- Row-only mutations update one existing row; membership changes rebuild one strict 50-row snapshot, preventing 49/51-row pagination drift.
- Message previews are capped at 280 Unicode scalar values, and reader shaping is capped at 16,384 values until the user explicitly loads an unusually large body.
- Local cache content renders immediately instead of constructing and discarding an artificial 650ms skeleton state.
- Production releases exclude the environment-driven stress harness. Benchmark it explicitly with `cargo build --release --features bench-harness`.
- Release profiles use full LTO, one codegen unit, stripped symbols, and abort-on-panic. The recommended `s` profile retained `3`-level stress throughput within 2.5% while reducing the measured stress RSS by about 2MiB.
- The active SQLite path returns at most 50 metadata rows, reads at most one 64KiB reader excerpt, stores large bodies and MIME payloads by private file reference, and caps the SQLite page cache at 1MiB. Mailbox pages include persistent counters and at most 64 account unread values without scanning for exact counts; raw import writes mark counters stale until an atomic rebuild completes. Local mutations serialize on the same actor, retain at most 256 folders in the single trash-undo slot, and queue zero-reference file keys for later deletion without loading file-key collections into Rust memory. Their remote journal snapshots remain in SQL and are capped at 4,096 targets per account, 16,384 globally, 65,536 child rows, and 320KiB per provider payload. The actor and Tokio core remain separate threads; no reply-bridge thread or periodic polling loop is created. Provider execution, synchronization merge/reconciliation, FTS, presentation/error mapping, and active-query interruption are still required before this path replaces the live UI repository.

The current schema-v8 Linux release gate was measured at `f639c4b` from a 19,506,296-byte production binary. Three fresh X11 runs at 1200x900 produced a worst stable sample of 37.80MiB RSS / 24.02MiB PSS / 20.78MiB USS; a separate 300-second idle soak held RSS exactly at 37.61MiB from 30 seconds onward with 0.00% interval CPU after 60 seconds. A 10,000-action run settled at 42.10/28.39/25.16MiB after five minutes, or +11.93%/+18.80%/+21.75%, and a 3840x2400 resize/restore settled at +80.71%/+63.54%/+0.11%. The current X11 Skia-software build therefore passes the 90MiB hard idle gate, the 50MiB idle target, and the less-than-100% settled-growth gate. Provider sessions and real mailbox synchronization remain outside this measurement and require a new gate when connected. See `memory-report.md` for exact hashes, limitations, historical platform references, and reproduction commands.

This is an interaction-complete local prototype with production runtime and persistence foundations. Local mutations, versioned single-intent claims, and fenced reports now form one bounded durable SQLite boundary. Protocol expansion is paused while release memory gates, FTS, presentation mapping, and explicit error feedback are completed. Provider adapters, synchronization merge/reconciliation, IMAP/SMTP, optional JMAP, MIME ingestion, and encrypted credentials resume only after that boundary is measured and the controller can switch away from its in-memory implementation consistently.

## License

Nivalis Mail is distributed under the **GNU General Public License, version 3 only** (`GPL-3.0-only`). Distributions and modified versions must comply with GPLv3, including its corresponding-source and license-notice requirements. See [`LICENSE`](LICENSE) for the complete license text.
