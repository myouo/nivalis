# Nivalis Mail

A task-first desktop mail client prototype built with Rust, Slint, and the Skia renderer. Its compact visual language is independently designed from Fluent-style desktop patterns: flat surfaces, thin outlines, stable navigation, and progressive disclosure without copying third-party code or assets.

## Technology

The confirmed application stack is Rust 2024 with Slint 1.17.1, Winit, and the Skia renderer. The production architecture keeps the native Slint UI on the main thread, runs network work on one Tokio current-thread runtime, and keeps SQLite behind one serialized writer actor plus one query-only UI reader actor. The active receive and send boundaries use `async-imap`, `mail-parser`, a custom streaming MIME writer, Lettre's SMTP transport, Rustls, and the `keyring-core` ecosystem. OAuth2 and optional JMAP remain later measured milestones. Nivalis does not embed a WebView.

The bounded Tokio core, schema-v15 persistence layer, keyset mailbox projections, body-aware FTS5, and two global SQLite connections are active. UI-visible account, mailbox, search, and selected-detail reads use the query-only connection, so background imports and index backfill cannot queue ahead of the first screen. Schema v15 keeps the hot `messages` table separate from search documents, adds sender/subject/body scopes and Chinese trigram search, and backfills old cached bodies in 16-row idle batches. Automatic IMAP metadata synchronization is local-first, fetches recent mail before history, loads bodies on demand, reuses selected TLS sessions, and maintains at most ten cancellable IDLE watches without a thread or runtime per account. Foreground sync and body fetches preempt IDLE and reclaim its authenticated session before doing network work. Local flags, Archive, Trash, drafts, the durable SMTP Outbox, and permanent deletion retain their existing transactional fences. See [`docs/architecture.md`](docs/architecture.md) for ownership boundaries and [`docs/performance.md`](docs/performance.md) for the current latency/resource matrix.

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

The default renderer uses GPU-accelerated Skia for fluid scrolling and interaction animation. If GPU initialization is unavailable, Slint automatically falls back to its software Skia surface. CPU rasterization can also be selected explicitly for compatibility or low-memory measurements:

```bash
NIVALIS_RENDERER=skia-software cargo run --release
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
- Local-first automatic and manual INBOX metadata sync, waterfall history loading, on-demand bodies, IMAP IDLE notification, visible progress, and SQLite-backed results
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
- Dialogs, menus, and settings are conditionally instantiated. IMAP IDLE watchers run as bounded futures on the existing core runtime; no polling thread or runtime is created per account.
- Ordinary mailbox, reader, and mutation work crosses bounded 64-command and 128-event channels without blocking the UI. Secret-bearing account operations use a separate capacity-four queue, compose uses a capacity-one queue, outbox and file-janitor wakeups each use capacity one, and lossy status hints use capacity 16 while SQLite remains authoritative; each janitor pass processes at most 16 durable candidates, and snackbar feedback alone uses a restartable UI timer.
- Full mailbox and reader projections never accumulate in the 128-slot control queue: it carries lightweight notifications while independent latest-value slots retain at most one 50-row page and one 64KiB detail.
- UI-visible mutations are single-flight and retain no history queue. A successful write fences obsolete reads and must commit one strict 50-row first-page snapshot before another ordinary action; Trash undo may replace an in-flight refresh without extending its absolute five-second deadline.
- Stored previews are capped at 2,048 UTF-8 bytes and the reader excerpt at 64KiB. A draft body is capped at 1MiB, recipients at 64, and generated outbound MIME at 8MiB. The outbox permits at most 128 active items, 128MiB of active artifacts, and 256 total records, while the UI materializes only the first 64 summaries. Full bodies, attachments, and outbound MIME stay in private files and are consumed through bounded streams rather than copied into SQLite or mailbox-wide UI state.
- Local cache content renders immediately instead of constructing and discarding an artificial 650ms skeleton state.
- Production releases exclude the environment-driven stress harness. Benchmark it explicitly with `cargo build --release --features bench-harness`.
- Release profiles use full LTO, one codegen unit, stripped symbols, and abort-on-panic. The recommended `s` profile retained `3`-level stress throughput within 2.5% while reducing the measured stress RSS by about 2MiB.
- The active SQLite path returns at most 50 metadata rows, reads at most one 64KiB reader excerpt, stores large bodies and MIME payloads by private file reference, and uses exactly two process-global file-database connections: a 1MiB-cache writer and a 512KiB-cache query-only UI reader. Mailbox pages include persistent counters and at most 64 account unread values without scanning for exact counts. Local mutations and sync commits serialize on the writer; UI reads never wait behind that queue. No database connection, reply-bridge thread, Tokio runtime, or OS thread is created per account. First/Next/Previous keyset navigation retains only the visible waterfall window and bounded cursors.

The last full release-RSS baseline is revision `24d616f8b947965acc32998438608d7714eaaf19` (38,760KiB empty-idle and 42,856KiB for the loopback send lifecycle). It predates the schema-v15 reader actor and IDLE path, so it is retained only as a regression reference. Current local/real-account latency and exact resource bounds are tracked in [`docs/performance.md`](docs/performance.md); a ten-live-account RSS/PSS soak remains outstanding rather than being inferred from a single account.

This is now a minimal receiving and managed-sending mail client: configured app-password accounts share one fair bounded scheduler for automatic INBOX pages, while a selected account can also sync manually, persist a plain-text draft, send it through the production SMTP stack, and recover durable delivery outcomes through the Outbox. M5 is closed by the release idle and account-send evidence above. M6 remains open for remote desired-state execution, bidirectional convergence, fair cross-account SMTP claiming, optional JMAP, and current release memory evidence. No simulated delivery success remains.

## License

Nivalis Mail is distributed under the **GNU General Public License, version 3 only** (`GPL-3.0-only`). Distributions and modified versions must comply with GPLv3, including its corresponding-source and license-notice requirements. See [`LICENSE`](LICENSE) for the complete license text.
