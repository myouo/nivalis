# Nivalis Mail

A task-first desktop mail client prototype built with Rust, Slint, and the Skia renderer. Its compact visual language is independently designed from Fluent-style desktop patterns: flat surfaces, thin outlines, stable navigation, and progressive disclosure without copying third-party code or assets.

## Technology

The confirmed application stack is Rust 2024 with Slint 1.17.1, Winit, and the Skia renderer. The production architecture keeps the native Slint UI on the main thread, runs network work on one Tokio current-thread runtime, and serializes SQLite access through a dedicated `rusqlite` actor. The active receive and send boundaries use `async-imap`, `mail-parser`, a custom streaming MIME writer, Lettre's SMTP transport, Rustls, and the `keyring-core` ecosystem. OAuth2 and optional JMAP remain later measured milestones. Nivalis does not embed a WebView.

The bounded Tokio core, schema-v13 persistence layer, keyset mailbox projections, external-content FTS, and single-connection SQLite actor are active. Local flags, Archive, Trash, Trash undo, and permanent deletion run as immediate transactions that compact bounded desired remote state with undo, tombstones, deferred file cleanup, and persistent statistics. Schema v12 introduced revision-fenced drafts plus a reservation-, artifact-, lease-, and DATA-fenced SMTP outbox; schema v13 repairs draft and Sent statistics left dirty by older v12 writes. The local content pipeline streams received bodies, attachments, draft bodies, and outbound MIME into private files instead of retaining mailbox-wide payloads. The visible controller reads accounts, the current 50-row mailbox page, persistent counters, one selected detail, and at most 64 persisted Outbox summaries exclusively from SQLite; request IDs and generations reject obsolete results. Users can add, diagnose, manually receive one bounded INBOX page, save a plain-text draft across restart, queue and send it through SMTP, cancel an active attempt, and resolve failed or uncertain delivery without losing the durable fence. An exact current-generation SMTP 535 rejection exposes an app-password replacement path before explicit retry, while closing a dirty composer durably saves it before allowing the window to exit. Automatic synchronization, folders beyond INBOX, fair multi-account scheduling, and broader provider coverage remain later work. See [`docs/architecture.md`](docs/architecture.md) for ownership boundaries, backpressure rules, dependency features, implementation status, and memory budgets.

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

## Local quality gates

Install the development-only Git hooks after cloning (Node.js `^20.17.0` or `>=22.9.0`):

```bash
npm ci
```

Husky runs `scripts/check-local.sh commit` before commits, merge commits, and applied patches: the index must match the worktree, formatting must be clean, and the default Rust test suite must pass. Before every push, `scripts/check-local.sh push` requires a clean worktree and a pushed commit matching the tested `HEAD`, then repeats those checks and also runs the benchmark-harness tests plus Clippy across all targets and features. The same commands can be run manually with `npm run test:commit` and `npm run test:push`; GitHub Actions remains the remote verification layer.

## Experience

- Frameless 40px title bar, 248px collapsible sidebar, 54px rail, and adaptive single-pane reading below 760px
- Inbox-first three-pane workspace with account-aware search and progressive folder filters
- Persistent SQLite account catalog plus an aggregated `All inboxes` view
- App-password IMAP account setup, bounded Rustls connection diagnostics, actionable status, and restart-safe removal
- Manual single-account INBOX receive with bounded paging, visible progress, actionable errors, and SQLite-backed results
- Restart-safe plain-text drafts and a 64-row persistent Outbox with retry, return-to-Drafts, uncertain-delivery review, active-attempt cancellation, and exact SMTP 535 app-password repair
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
- `ui/mailbox.slint`, `ui/reader.slint`, `ui/outbox.slint`, and `ui/states.slint` isolate the main task surfaces, persistent delivery management, and reusable empty state.
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
- Search uses a restartable 180ms debounce, a 256-byte input cap, generation rejection, external-content SQLite FTS, and exact-key interruption of obsolete mailbox work.
- Dialogs, menus, and settings are conditionally instantiated, and no decorative animation or periodic synchronization timer runs while idle.
- Ordinary mailbox, reader, and mutation work crosses bounded 64-command and 128-event channels without blocking the UI. Secret-bearing account operations use a separate capacity-four queue, compose uses a capacity-one queue, outbox and file-janitor wakeups each use capacity one, and lossy status hints use capacity 16 while SQLite remains authoritative; each janitor pass processes at most 16 durable candidates, and snackbar feedback alone uses a restartable UI timer.
- Full mailbox and reader projections never accumulate in the 128-slot control queue: it carries lightweight notifications while independent latest-value slots retain at most one 50-row page and one 64KiB detail.
- UI-visible mutations are single-flight and retain no history queue. A successful write fences obsolete reads and must commit one strict 50-row first-page snapshot before another ordinary action; Trash undo may replace an in-flight refresh without extending its absolute five-second deadline.
- Stored previews are capped at 2,048 UTF-8 bytes and the reader excerpt at 64KiB. A draft body is capped at 1MiB, recipients at 64, and generated outbound MIME at 8MiB. The outbox permits at most 128 active items, 128MiB of active artifacts, and 256 total records, while the UI materializes only the first 64 summaries. Full bodies, attachments, and outbound MIME stay in private files and are consumed through bounded streams rather than copied into SQLite or mailbox-wide UI state.
- Local cache content renders immediately instead of constructing and discarding an artificial 650ms skeleton state.
- Production releases exclude the environment-driven stress harness. Benchmark it explicitly with `cargo build --release --features bench-harness`.
- Release profiles use full LTO, one codegen unit, stripped symbols, and abort-on-panic. The recommended `s` profile retained `3`-level stress throughput within 2.5% while reducing the measured stress RSS by about 2MiB.
- The active SQLite path returns at most 50 metadata rows, reads at most one 64KiB reader excerpt, stores large bodies and MIME payloads by private file reference, and caps the SQLite page cache at 1MiB. Mailbox pages include persistent counters and at most 64 account unread values without scanning for exact counts; raw import writes mark counters stale until an atomic rebuild completes. Local mutations serialize on the same actor, retain at most 256 folders in the single trash-undo slot, and queue zero-reference file keys for later deletion without loading file-key collections into Rust memory. Their remote journal snapshots remain in SQL and are capped at 4,096 targets per account, 16,384 globally, 65,536 child rows, and 320KiB per provider payload. The actor and Tokio core remain separate threads; no reply-bridge thread or runtime per account is created. First/Next/Previous keyset navigation retains only the current 50-row page and two cursors. Manual receive, persistent Outbox management, and the one-global-claim SMTP drainer use that same bounded core; full synchronization merge/reconciliation remains required.

The current release-code checkpoint is `24d616f8b947965acc32998438608d7714eaaf19` on schema v13. Three production empty-idle runs peak at 38,760KiB RSS (37.85MiB). The loopback account-send lifecycle peaks and settles at 42,856KiB RSS (41.85MiB), then leaves the account, Outbox, file-GC queue, content tables, and private content directory empty. Both workloads record zero swap and finish at 0.00% CPU, passing the 90MiB hard gate and preferred 50MiB target. See [`memory-report.md`](memory-report.md) for raw samples, hashes, limitations, and reproduction commands.

This is now a minimal receiving and managed-sending mail client: one selected app-password account can fetch and display one bounded INBOX page, persist a plain-text draft, send it through the production SMTP stack, and recover durable delivery outcomes through the Outbox. M5 is closed by the release idle and account-send evidence above. It is not yet an automatic, full-folder, fairly scheduled multi-account client; M6 owns bounded multi-account scheduling and convergence. No simulated delivery success remains.

## License

Nivalis Mail is distributed under the **GNU General Public License, version 3 only** (`GPL-3.0-only`). Distributions and modified versions must comply with GPLv3, including its corresponding-source and license-notice requirements. See [`LICENSE`](LICENSE) for the complete license text.
