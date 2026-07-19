# Nivalis Mail Architecture

## Status and constraints

This document fixes the production architecture for Nivalis Mail. Components described as planned are not necessarily wired into the current prototype yet, but new production work must preserve these ownership and resource boundaries.

The primary constraints are predictable UI latency, safe processing of untrusted mail, reliable offline behavior, and a release idle resident set below 90 MiB, preferably below 50 MiB. Memory growth after representative synchronization must stabilize below twice the warm-idle baseline. A smaller feature graph and bounded live data take priority over speculative concurrency.

### Implementation status

- The UI-to-core boundary uses a bounded 64-command channel feeding one Tokio `current_thread` runtime. A bounded 128-slot control queue returns lightweight events through Slint's local executor; full mailbox and reader projections live in independent latest-value slots instead of accumulating in that queue.
- SQLite schema v8, atomic migrations, 50-row keyset projections, 64KiB reader excerpts, transactional local mutations, persistent mailbox statistics, remote identity storage, and a dedicated single-connection actor are implemented, tested, and constructed by `main` in a private per-user data directory.
- SQLite replies use a bounded Tokio channel that the core polls asynchronously. The actor retains cancellable backpressure without adding a reply-bridge thread or idle polling loop. Independent account, mailbox, and detail schedulers retain one active and one latest pending request, and an eight-command fairness budget prevents either commands or database replies from being starved.
- Flag, star, archive, trash, undo, and permanent-delete operations use immediate SQLite transactions. Mutation results are never coalesced; accepted writes are completed during Core and database-actor shutdown, and admission closes before either actor drains its queue.
- Mailbox statistics are one bounded row per account and are updated by fixed-width deltas in the same transaction as each local mutation. Raw message, membership, or folder-role writes mark the affected row dirty; queries reject stale values until the importing transaction performs an atomic rebuild. The schema rejects a sixty-fifth new account; statistic queries read at most 65 rows to detect legacy overflow and return at most 64 per-account values.
- Schema v8 provides bounded storage for mailbox-scoped IMAP locators, opaque account-scoped JMAP object states, compacted desired-state intents, frozen folder/locator snapshots, versioned lease metadata, and placement-rebase reservations. Local flags, placement changes, and terminal deletions reduce into this journal in the same immediate transaction as message state, undo/tombstone data, file-GC references, and statistics. Parent, child, reservation, and payload caps roll back the complete mutation on overflow; recursive triggers keep constant-time child and reservation usage counters consistent across replacement, recovery, and cascade paths. Legacy revisions retain indexed target-level reconciliation markers instead of unbounded migration backfill. The actor claims one complete intent and applies fully fenced reports through cancellable oneshot paths that bypass UI reply backpressure. Reports cover confirmation, satisfaction, progress renewal, retry, reconciliation, blocking, provider checkpoints, and merges with newer desired versions. Provider writes remain disabled until provider execution and synchronization merge/reconciliation are implemented.
- Trash undo retains one five-second snapshot with at most 256 folder memberships. Permanent deletion creates a tombstone and queues only file keys with no remaining database reference; an eventual janitor must revalidate the private path and zero-reference invariant immediately before unlinking.
- The visible controller still reads and mutates one `MailStore` instance and deliberately does not consume SQLite mutation events. M1 replaces that repository as one coherent controller cutover after bounded presentation and error mapping are ready; production must not retain two conflicting sources of truth. Simulated synchronization and in-memory delivery success are disabled while this cutover is in progress.
- Live SQLite search still requires FTS and interruption of the active obsolete query. The bounded `LIKE` implementation and generation coalescing validate semantics but are not the production large-mailbox search path.
- IMAP/SMTP, optional JMAP, MIME ingestion, OAuth2, keyring integration, and the controller's consistent SQLite cutover remain planned backend slices.

## Process topology

Nivalis uses three long-lived execution domains. They exchange typed messages and never share mutable protocol or database objects.

### Native UI thread

Slint, Winit, and Skia remain on the main thread. The UI owns presentation state, keyboard and accessibility state, and bounded visible models. It never performs network requests, MIME parsing, attachment I/O, token refresh, or SQLite queries synchronously.

The application does not embed Chromium, WebKit, or another WebView. Plain text is the safe baseline. A future HTML path must sanitize into a deliberately limited native document model, block remote resources by default, and open user-approved links in the system browser. This avoids a second browser process, an unbounded DOM, and an additional security boundary.

The Slint module graph is deliberately one-way. `models.slint` and `theme.slint` are leaves; `components/`, `shared.slint`, and `actions.slint` provide reusable presentation building blocks; `shell.slint`, `mailbox.slint`, `reader.slint`, `states.slint`, and `overlays.slint` own the currently enabled feature surfaces; `app.slint` is the only build entry and final composition root. Public Rust types are explicitly re-exported from that entry. The composer returns only with the durable outbox milestone.

`AppWindow` remains the sole owner of responsive breakpoints, cross-surface state, Escape-key priority, and Rust-facing callbacks. Feature components receive bounded projections and expose user intents through callbacks or two-way bindings. Menus, settings, confirmation dialogs, and snackbar surfaces remain behind `if` instantiation boundaries in `AppWindow`; splitting files must not turn hidden surfaces into always-live objects or alter focus and accessibility order.

### Bounded command and event boundary

UI intents become small `Command` values sent to the core through a bounded channel. Core control results become small event envelopes and are applied with Slint's event-loop bridge. Mailbox and selected-reader projections carry stable request identifiers and generations but are stored in separate latest-value slots, so a slow UI retains no more than one page and one detail.

Initial capacities are deliberately modest and must be validated by profiling: 64 UI-to-core commands and 128 core-to-UI events. Saturation is handled by policy rather than queue growth:

- Search, selection, progress, unread-count, and provider status updates are replaceable and coalesced to the newest value.
- Destructive actions, sends, authentication results, and durable state transitions are never silently dropped.
- Producers await capacity or return an explicit busy result; no production path may use an unbounded channel.
- Cancellation tokens terminate obsolete search, fetch, and account-sync work before their results reach the UI.

Only the selected message may materialize a full reader model. Mailbox pages remain capped at 50 summaries, matching the current virtualized `ListView` contract.

### Tokio network actor

One background OS thread owns one Tokio `current_thread` runtime. All account state machines share that runtime; the application must not create a runtime or worker pool per account. Tokio features are limited to `rt`, `net`, `io-util`, `sync`, and `time` unless a measured requirement justifies another feature.

The network actor owns IMAP/JMAP sessions, SMTP submissions, OAuth token refresh, connection timeouts, cancellation, and reconnect backoff. Active IMAP IDLE connections are capped and scheduled across accounts. SMTP connections exist only while the durable outbox is draining. JMAP uses state/change requests by default and does not keep both EventSource and WebSocket transports alive.

### SQLite actor

A separate blocking thread owns the single `rusqlite::Connection`; neither the UI nor Tokio tasks access it directly. Crossbeam provides bounded requests and lifecycle signals, while bounded Tokio replies wake the core directly without another thread. The actor performs migrations, mailbox queries, local mail transactions, remote-intent claims, and fenced reports serially. Report requests use independent oneshot replies: `Busy`, `Closed`, and execution failures return the exact submission, while accepted reports still execute after receiver cancellation, UI reply closure, or shutdown drain. SMTP outbox draining and provider execution are not implemented yet. Submission and shutdown share an admission gate, so a request cannot report success after draining begins. FTS and remote journal execution remain on this same actor rather than adding persistence threads.

SQLite stores accounts, multi-folder message membership, metadata, flags, synchronization cursors and object states, protocol-safe remote locators, a bounded desired-state journal, a durable SMTP envelope/outbox, compact searchable text, tombstones, bounded undo state, persistent account statistics, and file references. Large bodies, attachments, and outbound MIME are streamed to private bounded files and loaded on demand instead of being retained as database blobs or process-wide byte vectors. Queries return keyset-paged projections and fixed-size persisted counters, never the full mailbox or an exact count scan on every page. WAL, recursive triggers, file permissions, query limits, worker count, mmap, cache sizing, and write-drain behavior are explicit and covered by tests.

## Mail backends

The core exposes one provider-neutral mailbox interface. IMAP/SMTP is the required backend; JMAP is compiled and configured independently so providers can be selected without leaking protocol types into UI models.

Remote identity, desired-state compaction, versioned claim/report handling, and conflict rules are fixed by [`remote-sync-contract.md`](remote-sync-contract.md). Provider adapters may be added only behind that contract; an IMAP UID by itself is never treated as an account-global message identity.

### IMAP and SMTP

`async-imap` is the production IMAP client and is built with default features disabled and only its Tokio runtime integration. The application owns capability discovery, UIDVALIDITY handling, incremental synchronization, IDLE lifecycle, reconnect policy, and provider compatibility tests. IMAP compression is disabled until measurements demonstrate a net benefit.

`lettre` provides SMTP submission and outbound MIME construction. Its defaults are disabled; the intended features are `builder`, `smtp-transport`, `tokio1`, `tokio1-rustls`, `ring`, and `rustls-platform-verifier`. SMTP pooling and client-side DKIM are off by default. The SQLite outbox, not an in-memory task, is the source of truth for retries and delivery state.

### Optional JMAP

`jmap-client` is an optional backend for servers that advertise compatible JMAP support. It uses bounded result pages, explicit property selection, and change tokens. WebSocket support is disabled by default; polling `/changes` is the low-residency baseline. Blob downloads must stream to disk with declared and enforced size limits.

JMAP does not replace IMAP for general provider coverage. It must remain behind a Cargo feature and the provider-neutral interface so installations that do not use it do not carry its HTTP, JSON, or WebSocket dependency cost.

## Message parsing and content safety

`mail-parser` parses received MIME using borrowed data where possible. International charset support is enabled through `full_encoding`, but owned conversion of the complete message is avoided. Parsing runs outside the UI thread and is subject to configured limits for raw message size, MIME depth, part count, header bytes, and total decoded bytes. Attachments decode directly to temporary files before an atomic cache move.

Only envelope fields and a bounded preview are loaded for lists. Full MIME structures live only while a message is open or an operation explicitly needs them, then are dropped. Remote images, scripts, active content, `data:` payloads, and automatic external navigation are not part of the native renderer.

## Authentication, TLS, and secrets

The `oauth2` crate implements Authorization Code with PKCE for desktop sign-in and Device Authorization where a provider requires it. OAuth HTTP requests use the shared asynchronous HTTP/TLS stack, reject redirects at the token client, verify `state`, and request the minimum provider scopes. IMAP uses XOAUTH2 or OAUTHBEARER through its authenticator; SMTP uses Lettre's XOAUTH2 support.

Rustls is the only TLS implementation. `tokio-rustls` supplies asynchronous streams and `rustls-platform-verifier` uses the operating-system trust policy. The process installs one `ring` crypto provider. Certificate verification cannot be disabled outside isolated tests, and native-tls/OpenSSL must not enter the release dependency graph accidentally.

The `keyring` crate stores refresh tokens and app passwords in the operating-system credential service, keyed by an opaque account identifier. Access tokens are short-lived in-memory values. Secrets are excluded from SQLite, UI models, diagnostics, panic messages, and tracing fields. Account removal deletes keyring entries and invalidates live protocol sessions.

## Memory contract

Memory is measured from stripped release builds after startup and again after a representative multi-account sync/open/close cycle. `memory-report.md` defines the current Linux measurement procedure and remains the reproducible baseline.

The latest measured X11 baseline is schema v8 at revision `f639c4b`, built as a 19,506,296-byte stripped release. Across three fresh 1200x900 Skia-software runs, the worst stable sample was 37.80 MiB RSS / 24.02 MiB PSS / 20.78 MiB USS. A separate 300-second idle soak held RSS exactly at 37.61 MiB from 30 seconds onward and returned to 0.00% interval CPU. A 10,000-action, five-minute run settled at 42.10/28.39/25.16 MiB, or +11.93%/+18.80%/+21.75%, while a 3840x2400 resize and restore settled at +80.71%/+63.54%/+0.11%. This proves the current X11 Skia-software UI/core/SQLite build against the idle and growth gates on the reference host. It does not cover future IMAP/JMAP sessions, MIME/attachment processing, or representative multi-account synchronization; those paths require a new soak before provider-enabled release gating.

The release gates are:

- Warm idle RSS is preferably at or below 50 MiB and must remain below 90 MiB on supported reference environments.
- Stabilized post-workload RSS must remain below 200% of the warm-idle value; transient peaks are separately reported and constrained by input limits.
- Visible mailbox data is limited to 50 summaries, one selected reader model, and bounded compose/reply state.
- Commands, events, database work, MIME input, decoded content, and download concurrency all have explicit finite limits.
- Closing a reader, composer, account, or synchronization job releases its body buffers, parser trees, tasks, and connections.

RSS, PSS, and USS are recorded because Skia, font mappings, system libraries, and shared pages affect them differently. A regression is not waived solely because it is shared memory. Heap profiles and `smaps_rollup` snapshots should identify whether growth belongs to Skia caches, SQLite page caches, protocol buffers, MIME content, or allocator retention.

## Cargo feature policy

Every substantial dependency is added with `default-features = false`; release features are allow-listed. The intended policy is:

- Slint keeps only `std`, Winit, Skia, accessibility, the required Winit compatibility layer, and `compat-1-2`.
- Tokio uses a single current-thread runtime and no multithread scheduler, macros, process, signal, or filesystem feature unless required and measured.
- Rusqlite disables defaults and enables only the bundled SQLite build plus runtime limits; one actor owns one connection with a 1MiB cache, mmap disabled, and SQLite worker threads set to zero.
- Crossbeam channel defaults are disabled and only `std` is enabled for bounded database requests, startup, and shutdown signals; bounded Tokio `mpsc` carries replies into the async core.
- `async-imap` uses `runtime-tokio`; async-std and IMAP compression stay out of the baseline.
- Lettre uses only message building, SMTP, Tokio/Rustls, `ring`, and the platform verifier; pooling and DKIM stay out.
- JMAP and any WebSocket transport are optional, with WebSocket disabled in the normal build.
- OAuth2 reuses the application HTTP client; duplicate reqwest, native-tls, OpenSSL, and certificate-root stacks are forbidden.
- Mail parsing enables only required charset support; serialization formats not used by the cache remain off.
- The benchmark harness remains opt-in and excluded from production releases.

CI must inspect `cargo tree -e features` and `cargo tree -d` for duplicate runtimes, TLS implementations, crypto providers, and major HTTP versions. Any new default feature needs a measured memory cost, an owner, and a removal criterion. Release profiles retain LTO, one codegen unit, stripped symbols, and abort-on-panic unless profiling demonstrates a better whole-application tradeoff.
