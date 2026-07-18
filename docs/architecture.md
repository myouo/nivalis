# Nivalis Mail Architecture

## Status and constraints

This document fixes the production architecture for Nivalis Mail. Components described as planned are not necessarily wired into the current prototype yet, but new production work must preserve these ownership and resource boundaries.

The primary constraints are predictable UI latency, safe processing of untrusted mail, reliable offline behavior, and a release idle resident set below 90 MiB, preferably below 50 MiB. Memory growth after representative synchronization must stabilize below twice the warm-idle baseline. A smaller feature graph and bounded live data take priority over speculative concurrency.

## Process topology

Nivalis uses three long-lived execution domains. They exchange typed messages and never share mutable protocol or database objects.

### Native UI thread

Slint, Winit, and Skia remain on the main thread. The UI owns presentation state, keyboard and accessibility state, and bounded visible models. It never performs network requests, MIME parsing, attachment I/O, token refresh, or SQLite queries synchronously.

The application does not embed Chromium, WebKit, or another WebView. Plain text is the safe baseline. A future HTML path must sanitize into a deliberately limited native document model, block remote resources by default, and open user-approved links in the system browser. This avoids a second browser process, an unbounded DOM, and an additional security boundary.

The Slint module graph is deliberately one-way. `models.slint` and `theme.slint` are leaves; `components/`, `shared.slint`, and `actions.slint` provide reusable presentation building blocks; `shell.slint`, `mailbox.slint`, `reader.slint`, `composer.slint`, `states.slint`, and `overlays.slint` own feature surfaces; `app.slint` is the only build entry and final composition root. Public Rust types are explicitly re-exported from that entry.

`AppWindow` remains the sole owner of responsive breakpoints, cross-surface state, Escape-key priority, and Rust-facing callbacks. Feature components receive bounded projections and expose user intents through callbacks or two-way bindings. Menus, settings, the composer, confirmation dialogs, and snackbar surfaces remain behind `if` instantiation boundaries in `AppWindow`; splitting files must not turn hidden surfaces into always-live objects or alter focus and accessibility order.

### Bounded command and event boundary

UI intents become small `Command` values sent to the core through a bounded channel. Core results become small `Event` values sent back through another bounded channel and are applied with Slint's event-loop bridge. Messages carry stable identifiers, progress, and compact projections rather than message bodies or attachments.

Initial capacities are deliberately modest and must be validated by profiling: 64 UI-to-core commands and 128 core-to-UI events. Saturation is handled by policy rather than queue growth:

- Search, selection, progress, unread-count, and sync-status updates are replaceable and coalesced to the newest value.
- Destructive actions, sends, authentication results, and durable state transitions are never silently dropped.
- Producers await capacity or return an explicit busy result; no production path may use an unbounded channel.
- Cancellation tokens terminate obsolete search, fetch, and account-sync work before their results reach the UI.

Only the selected message may materialize a full reader model. Mailbox pages remain capped at 50 summaries, matching the current virtualized `ListView` contract.

### Tokio network actor

One background OS thread owns one Tokio `current_thread` runtime. All account state machines share that runtime; the application must not create a runtime or worker pool per account. Tokio features are limited to `rt`, `net`, `io-util`, `sync`, and `time` unless a measured requirement justifies another feature.

The network actor owns IMAP/JMAP sessions, SMTP submissions, OAuth token refresh, connection timeouts, cancellation, and reconnect backoff. Active IMAP IDLE connections are capped and scheduled across accounts. SMTP connections exist only while the durable outbox is draining. JMAP uses state/change requests by default and does not keep both EventSource and WebSocket transports alive.

### SQLite actor

A separate blocking thread owns the single `rusqlite::Connection`; neither the UI nor Tokio tasks access it directly. Bounded database requests use typed request/reply messages. The actor performs transactions, migrations, mailbox queries, FTS work, and durable outbox updates serially.

SQLite stores accounts, folders, message metadata, flags, synchronization cursors, compact searchable text, and file references. Large bodies and attachments are streamed to bounded files and loaded on demand instead of being retained as database blobs or process-wide byte vectors. Queries return paged projections, never the full mailbox. WAL and cache sizing must be set explicitly and included in memory measurements.

## Mail backends

The core exposes one provider-neutral mailbox interface. IMAP/SMTP is the required backend; JMAP is compiled and configured independently so providers can be selected without leaking protocol types into UI models.

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
- `async-imap` uses `runtime-tokio`; async-std and IMAP compression stay out of the baseline.
- Lettre uses only message building, SMTP, Tokio/Rustls, `ring`, and the platform verifier; pooling and DKIM stay out.
- JMAP and any WebSocket transport are optional, with WebSocket disabled in the normal build.
- OAuth2 reuses the application HTTP client; duplicate reqwest, native-tls, OpenSSL, and certificate-root stacks are forbidden.
- Mail parsing enables only required charset support; serialization formats not used by the cache remain off.
- The benchmark harness remains opt-in and excluded from production releases.

CI must inspect `cargo tree -e features` and `cargo tree -d` for duplicate runtimes, TLS implementations, crypto providers, and major HTTP versions. Any new default feature needs a measured memory cost, an owner, and a removal criterion. Release profiles retain LTO, one codegen unit, stripped symbols, and abort-on-panic unless profiling demonstrates a better whole-application tradeoff.
