# Nivalis Mail

A task-first desktop mail client prototype built with Rust, Slint, and the Skia renderer. Its compact visual language is independently designed from Fluent-style desktop patterns: flat surfaces, thin outlines, stable navigation, and progressive disclosure without copying third-party code or assets.

## Technology

The confirmed application stack is Rust 2024 with Slint 1.17.1, Winit, and the Skia renderer. The production architecture keeps the native Slint UI on the main thread, runs network work on one Tokio current-thread runtime, and serializes SQLite access through a dedicated `rusqlite` actor. Mail transport uses IMAP and SMTP, with JMAP as an optional provider backend; MIME parsing, OAuth2, Rustls, and the `keyring` crate complete the protocol and credential boundary. Nivalis does not embed a WebView.

The bounded Tokio core, schema-v8 persistence layer, keyset mailbox projections, and single-connection SQLite actor are now active. Local flags, Archive, Trash, Trash undo, and permanent deletion run as immediate transactions that compact the latest desired remote state with bounded undo, tombstones, deferred file cleanup, and persistent mailbox-statistic deltas in the same commit. Schema v8 stores mailbox-scoped IMAP locators, account-scoped JMAP state, a bounded desired-state journal, versioned lease metadata, and placement-rebase capacity reservations. The actor exposes bounded account-directory, mailbox, and reader projections plus fully fenced remote claim/report transactions. SQLite replies enter the core through a bounded asynchronous channel; account, mailbox, and selected-message results use independent latest-value slots, while mutation results remain ordered control events and accepted writes are drained during shutdown. The visible controller reads accounts, the current 50-row mailbox page, persistent counters, and one selected detail exclusively from SQLite. It now routes local flags, Archive, Trash, permanent deletion, and absolute-deadline Trash undo through the ordered mutation path, then accepts only an authoritative first-page refresh before enabling another ordinary action. Request IDs and view generations reject obsolete results, and an empty database shows an honest no-account state. Provider execution remains disabled until its protocol milestone. See [`docs/architecture.md`](docs/architecture.md) for ownership boundaries, backpressure rules, dependency features, implementation status, and memory budgets.

The staged path from this foundation to a real low-residency mail client is tracked in [`docs/roadmap.md`](docs/roadmap.md). A milestone is complete only when its production path, failure states, tests, CI, and relevant release-memory workload pass; simulated UI behavior is not counted as provider functionality.

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
- Persistent SQLite account catalog plus an aggregated `All inboxes` view
- Bounded local-cache browsing, folder filtering, search, and selected-message reading
- Durable local star, unread, Archive, Trash, permanent-delete, and Trash-undo actions with busy, retry, and failure feedback
- Independent account/mailbox/reader loading, no-account, empty, search-empty, disabled, and actionable error states
- Light, dark, high-contrast, and reduced-motion display preferences
- A 110KB Material Symbols subset, system Noto Sans typography, 48px interaction targets, focus rings, semantic accessibility labels, and live-region feedback

## Keyboard

- `Tab` and `Shift+Tab` move through controls.
- `Enter` or `Space` activates focused custom controls.
- `Escape` closes the active menu, dialog, or compact reading view.

## Structure

- `ui/app.slint` is the single Slint build entry and owns the stable Rust-facing window API, responsive state, keyboard routing, and conditional composition.
- `ui/models.slint` and `ui/theme.slint` define shared UI data types and visual tokens without depending on feature views.
- `ui/components.slint` is a compatibility facade over the primitives, controls, inputs, navigation, and feedback modules in `ui/components/`.
- `ui/shared.slint`, `ui/actions.slint`, and `ui/shell.slint` contain cross-feature presentation elements, reader actions, the title bar, and adaptive navigation.
- `ui/mailbox.slint`, `ui/reader.slint`, and `ui/states.slint` isolate the main task surfaces and reusable empty state.
- `ui/overlays.slint` contains menus, settings, confirmation, and snackbar surfaces; `ui/app.slint` keeps their `if` boundaries so hidden overlays are not instantiated.
- `src/main.rs` selects the process modules and starts the Slint event loop.
- `src/platform.rs` owns renderer selection and native window integration.
- `src/controller.rs` binds user intents to the current application services.
- `src/core/` owns the bounded command/event protocol and single-thread Tokio runtime.
- `src/presentation.rs` projects bounded SQLite DTOs into Slint models.
- `src/benchmark.rs` contains the opt-in memory stress harness.
- `src/store/sqlite/` owns the production schema, projections, transactional mutations, migrations, and dedicated database actor. `memory.rs` is compiled only for its focused tests.

The embedded icon subset is generated from Material Symbols Rounded and retains its upstream license in `assets/licenses`. Text uses the system's Noto Sans installation, with normal platform fallback behavior.

## Resource strategy

- Slint is built with `default-features = false` and only the Winit, Skia, accessibility, and compatibility features required by this app.
- The visible model is capped at 50 lightweight summaries and stays virtualized through `ListView`; only the selected message materializes one bounded reader detail.
- One SQLite mailbox query produces the bounded page, persistent folder counters, and at most 64 per-account unread counts; each bounded account-directory reply replaces only the 64-row catalog model.
- DTO text is converted into `SharedString` only for the current bounded Slint page or selected detail; the source DTO is then released.
- Search uses a restartable 180ms debounce, a 256-byte input cap, generation rejection, and schema-v9 external-content SQLite FTS over bounded summary fields. Active-query interruption remains M1 work.
- Dialogs, menus, and settings are conditionally instantiated, and no decorative animation or periodic synchronization timer runs while idle.
- Account, mailbox, reader, and mutation work crosses bounded 64-command and 128-event channels without blocking the UI; snackbar feedback alone uses a restartable UI timer.
- Full mailbox and reader projections never accumulate in the 128-slot control queue: it carries lightweight notifications while independent latest-value slots retain at most one 50-row page and one 64KiB detail.
- UI-visible mutations are single-flight and retain no history queue. A successful write fences obsolete reads and must commit one strict 50-row first-page snapshot before another ordinary action; Trash undo may replace an in-flight refresh without extending its absolute five-second deadline.
- Stored previews are capped at 2,048 UTF-8 bytes and the reader excerpt at 64KiB. Full-body loading stays unavailable until the bounded file-content pipeline is connected.
- Local cache content renders immediately instead of constructing and discarding an artificial 650ms skeleton state.
- Production releases exclude the environment-driven stress harness. Benchmark it explicitly with `cargo build --release --features bench-harness`.
- Release profiles use full LTO, one codegen unit, stripped symbols, and abort-on-panic. The recommended `s` profile retained `3`-level stress throughput within 2.5% while reducing the measured stress RSS by about 2MiB.
- The active SQLite path returns at most 50 metadata rows, reads at most one 64KiB reader excerpt, stores large bodies and MIME payloads by private file reference, and caps the SQLite page cache at 1MiB. Mailbox pages include persistent counters and at most 64 account unread values without scanning for exact counts; raw import writes mark counters stale until an atomic rebuild completes. Local mutations serialize on the same actor, retain at most 256 folders in the single trash-undo slot, and queue zero-reference file keys for later deletion without loading file-key collections into Rust memory. Their remote journal snapshots remain in SQL and are capped at 4,096 targets per account, 16,384 globally, 65,536 child rows, and 320KiB per provider payload. The actor and Tokio core remain separate threads; no reply-bridge thread or periodic polling loop is created. First/Next/Previous keyset navigation retains only the current 50-row page and two cursors. Active-query interruption, provider execution, and synchronization merge/reconciliation remain required.

The current schema-v8 SQLite-controller gate was measured at release-code revision `0d3453c` with a 19,410,424-byte production binary and a fully checked bounded fixture: 64 accounts, 51 messages, a full 50-row page plus one-row continuation, 2KiB previews, and 64KiB details. Three fresh 1200x900 X11 processes stayed at or below 38,408KiB (37.51MiB) RSS/VmHWM. Two independent 10,000-transition keyset-pagination runs settled at 38,364KiB and 38,336KiB RSS, with +0.68% and +0.75% growth. This proves the 90MiB hard gate and less-than-100% growth gate for the local-read and pagination slice. Repeated normal-idle runs meet the preferred 50MiB target, but a retained 68.62MiB historical outlier prevents treating that target as an unconditional guarantee. See `memory-report.md` for raw samples, hashes, limitations, and reproduction commands.

This is a persistent local-cache mail manager with production runtime and storage foundations, not yet a connected mail client. Simulated synchronization and delivery success are intentionally absent. SQLite is the sole source of visible mailbox truth; the remaining M1 work is precise interruption of obsolete searches, controller integration coverage, and fresh release-memory coverage of the activated write/search paths. Provider work remains paused until those gates pass.

## License

Nivalis Mail is distributed under the **GNU General Public License, version 3 only** (`GPL-3.0-only`). Distributions and modified versions must comply with GPLv3, including its corresponding-source and license-notice requirements. See [`LICENSE`](LICENSE) for the complete license text.
