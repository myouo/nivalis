# Nivalis Mail

A task-first desktop mail client prototype built with Rust, Slint, and the Skia renderer. Its compact visual language is independently designed from Fluent-style desktop patterns: flat surfaces, thin outlines, stable navigation, and progressive disclosure without copying third-party code or assets.

## Technology

The confirmed application stack is Rust 2024 with Slint 1.17.1, Winit, and the Skia renderer. The production architecture keeps the native Slint UI on the main thread, runs network work on one Tokio current-thread runtime, and serializes SQLite access through a dedicated `rusqlite` actor. Mail transport uses IMAP and SMTP, with JMAP as an optional provider backend; MIME parsing, OAuth2, Rustls, and the `keyring` crate complete the protocol and credential boundary. Nivalis does not embed a WebView.

The bounded Tokio core, schema-v10 persistence layer, keyset mailbox projections, external-content FTS, and single-connection SQLite actor are active. Local flags, Archive, Trash, Trash undo, and permanent deletion run as immediate transactions that compact bounded desired remote state with undo, tombstones, deferred file cleanup, and persistent statistics. Schema v10 also provides generation-fenced content replacement, bounded staging manifests, and delayed orphan collection. The local content pipeline bounds untrusted MIME work, streams normalized bodies and attachments into private files, opens them without materializing mailbox-wide buffers, and rechecks every SQLite reference before removal. The visible controller reads accounts, the current 50-row mailbox page, persistent counters, and one selected detail exclusively from SQLite; request IDs and view generations reject obsolete results. Provider execution remains disabled until its protocol milestone. See [`docs/architecture.md`](docs/architecture.md) for ownership boundaries, backpressure rules, dependency features, implementation status, and memory budgets.

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
- `src/content.rs` owns bounded MIME projection, private staging/publication, stream-open, and safe file removal.
- `src/store/sqlite/` owns the production schema, projections, transactional mutations, migrations, and dedicated database actor. `memory.rs` is compiled only for its focused tests.

The embedded icon subset is generated from Material Symbols Rounded and retains its upstream license in `assets/licenses`. Text uses the system's Noto Sans installation, with normal platform fallback behavior.

## Resource strategy

- Slint is built with `default-features = false` and only the Winit, Skia, accessibility, and compatibility features required by this app.
- The visible model is capped at 50 lightweight summaries and stays virtualized through `ListView`; only the selected message materializes one bounded reader detail.
- One SQLite mailbox query produces the bounded page, persistent folder counters, and at most 64 per-account unread counts; each bounded account-directory reply replaces only the 64-row catalog model.
- DTO text is converted into `SharedString` only for the current bounded Slint page or selected detail; the source DTO is then released.
- Search uses a restartable 180ms debounce, a 256-byte input cap, generation rejection, schema-v10 external-content SQLite FTS, and exact-key interruption of obsolete mailbox work.
- Dialogs, menus, and settings are conditionally instantiated, and no decorative animation or periodic synchronization timer runs while idle.
- Account, mailbox, reader, and mutation work crosses bounded 64-command and 128-event channels without blocking the UI; snackbar feedback alone uses a restartable UI timer.
- Full mailbox and reader projections never accumulate in the 128-slot control queue: it carries lightweight notifications while independent latest-value slots retain at most one 50-row page and one 64KiB detail.
- UI-visible mutations are single-flight and retain no history queue. A successful write fences obsolete reads and must commit one strict 50-row first-page snapshot before another ordinary action; Trash undo may replace an in-flight refresh without extending its absolute five-second deadline.
- Stored previews are capped at 2,048 UTF-8 bytes and the reader excerpt at 64KiB. Full bodies and attachments stay in private files and are consumed through bounded streams rather than copied into SQLite or mailbox-wide UI state.
- Local cache content renders immediately instead of constructing and discarding an artificial 650ms skeleton state.
- Production releases exclude the environment-driven stress harness. Benchmark it explicitly with `cargo build --release --features bench-harness`.
- Release profiles use full LTO, one codegen unit, stripped symbols, and abort-on-panic. The recommended `s` profile retained `3`-level stress throughput within 2.5% while reducing the measured stress RSS by about 2MiB.
- The active SQLite path returns at most 50 metadata rows, reads at most one 64KiB reader excerpt, stores large bodies and MIME payloads by private file reference, and caps the SQLite page cache at 1MiB. Mailbox pages include persistent counters and at most 64 account unread values without scanning for exact counts; raw import writes mark counters stale until an atomic rebuild completes. Local mutations serialize on the same actor, retain at most 256 folders in the single trash-undo slot, and queue zero-reference file keys for later deletion without loading file-key collections into Rust memory. Their remote journal snapshots remain in SQL and are capped at 4,096 targets per account, 16,384 globally, 65,536 child rows, and 320KiB per provider payload. The actor and Tokio core remain separate threads; no reply-bridge thread or periodic polling loop is created. First/Next/Previous keyset navigation retains only the current 50-row page and two cursors. Provider execution and synchronization merge/reconciliation remain required.

The current M2 gate was measured at release-code revision `8c005c8` with a 19,487,096-byte production binary and the checked 64-account, 51-message fixture. Three fresh 1200x900 X11 production processes peaked at 38,492KiB (37.59MiB) RSS and returned to 0.00% CPU. A 10,000-cycle workload completed 10,000 bounded MIME imports, body streams, attachment streams, and GC runs in 35.515 seconds; it peaked at 39,684KiB (38.75MiB), settled at +2.57% RSS/+3.51% PSS with zero swap, and left only the current body and attachment. This proves the hard 90MiB gate and preferred 50MiB target for the measured M2 matrix, while a retained historical outlier still prevents an unconditional 50MiB guarantee. See [`memory-report.md`](memory-report.md) for raw samples, hashes, limitations, and reproduction commands.

This is a persistent local-cache mail manager with production runtime and storage foundations, not yet a connected mail client. Simulated synchronization and delivery success are intentionally absent. M1 established SQLite as the sole source of visible mailbox truth; M2 completes the bounded local MIME/file lifecycle and its release-memory gate. M3 now owns real account setup and the credential boundary, followed by receive synchronization in M4.

## License

Nivalis Mail is distributed under the **GNU General Public License, version 3 only** (`GPL-3.0-only`). Distributions and modified versions must comply with GPLv3, including its corresponding-source and license-notice requirements. See [`LICENSE`](LICENSE) for the complete license text.
