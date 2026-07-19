# Nivalis Mail Delivery Roadmap

## Product goal

Nivalis Mail will be a reliable, multi-account desktop mail client built with Rust, Slint, and Skia. It must receive, search, read, organize, compose, and send real mail without a WebView while keeping its release working set predictable.

The reference Linux release gate is a warm-idle RSS below 90 MiB, with 50 MiB as the target at 1200x900 using the default Skia software renderer. Settled RSS and PSS after a bounded workload must remain below twice their pre-workload baselines. A milestone cannot inherit an earlier memory result when it activates a previously unmeasured dependency or execution path.

## Delivery rules

- Each milestone ends in a usable vertical slice with explicit success, empty, loading, busy, and failure behavior where applicable.
- UI-visible state has one durable source of truth. Demo or simulated behavior must not be presented as a completed production operation.
- Mailbox pages, bodies, attachments, channels, database work, network concurrency, retries, and caches have enforced finite limits.
- Large content is streamed to private files. Mailbox-wide collections, unbounded queues, per-account runtimes, and always-live hidden surfaces are forbidden.
- Every implementation change is committed atomically with its focused tests. Formatting, tests, Clippy, release build, and the relevant memory workload must pass before a milestone closes.
- Protocol expansion remains paused until the local SQLite vertical slice and its memory gate are complete.

## M0: bounded foundation

Status: complete.

- Modular native Slint interface with adaptive navigation, reader, composer, feedback states, themes, keyboard support, and accessibility semantics.
- Winit + Skia software default renderer, bounded presentation models, and conditional overlay instantiation.
- One Tokio current-thread core, one SQLite actor, bounded command/reply channels, fair scheduling, cancellation, and shutdown draining.
- SQLite schema v9 with keyset queries, external-content FTS, bounded excerpts, transactional local mutations, persistent statistics, durable remote desired state, versioned claim/report fencing, and placement-rebase reservations.
- Rust 1.95 CI and a measured schema-v8 release baseline below the idle and settled-growth targets.

## M1: SQLite single source of truth

Status: in progress.

Checkpoint: SQLite accounts, bounded bidirectional keyset pages, persistent counters, selected details, ordered local writes/undo, external-content FTS, and exact-key obsolete-query interruption now drive the production UI. A file-backed vertical test covers their success, empty, and repairable failure states. The `0d3453c` release proves the 90MiB hard idle gate, repeated normal runs below the preferred 50MiB target, and less-than-2x growth across two exact-count 10,000-transition pagination soaks, but it predates the activated write/search paths. Fresh release-memory coverage of those paths remains before M1 can close; the retained historical outlier still prevents an unconditional 50MiB guarantee.

Recorded follow-ups that do not block M1: evaluate a resumable batched FTS rebuild before supporting upgrades of very large pre-existing databases, and profile a CJK-aware or trigram tokenizer before promising arbitrary substring search. The current migration is atomic and the current search contract is Unicode case-folded literal phrase matching.

Acceptance criteria:

- The production controller no longer constructs or mutates `MailStore::demo()`.
- Account catalog, mailbox pages, selected-message details, local flags, Archive, Trash, permanent deletion, and Trash undo travel through the bounded core and SQLite actor.
- SQLite DTOs map to stable Slint models without truncating database identifiers or copying mailbox-wide data.
- Mailbox navigation supports real keyset pagination. Obsolete page/detail/search work is coalesced or interrupted.
- Search uses bounded SQLite FTS with migration, rebuild, update, delete, escaping, and query-plan tests.
- Empty databases show an honest account/onboarding state. Compose/send remains unavailable until the durable outbox milestone rather than reporting a simulated send.
- Busy, unavailable, conflict, resource-limit, not-found, loading, and retry feedback are mapped to actionable UI states.
- Unit, actor, core, presentation, and controller integration tests pass. Release idle and a long interaction soak remain within the memory contract.

## M2: bounded local content pipeline

Status: pending.

Acceptance criteria:

- MIME parsing enforces raw size, header, part-count, nesting, decoded-byte, and quoted-history limits.
- Metadata and bounded previews commit to SQLite while raw bodies and attachments stream to private temporary files followed by atomic moves.
- Opening one message materializes only its bounded native reader model; closing it releases parsing and body buffers.
- Attachment access validates ownership and paths. The file-GC janitor rechecks database references immediately before unlinking.
- Malformed and adversarial MIME fixtures cannot escape limits or cause unbounded resident growth.

## M3: accounts and security boundary

Status: pending.

Acceptance criteria:

- Users can add, diagnose, update, disable, and remove accounts without exposing secrets to SQLite or UI models.
- OAuth2 PKCE/device flows and application passwords use the operating-system keyring; access tokens remain short lived.
- Rustls and platform trust verification are mandatory. Authentication, permission, certificate, timeout, and offline failures have distinct recovery guidance.
- Account setup and connection diagnostics are cancellable, bounded, and covered by integration tests.

## M4: IMAP receive and synchronization

Status: pending.

Acceptance criteria:

- Capability discovery, folder discovery, UIDVALIDITY, incremental fetch, reconnect backoff, and bounded IMAP IDLE are implemented.
- Incoming state, locators, cursors, persistent statistics, legacy reconciliation, tombstones, and pending local desired dimensions merge atomically.
- Provider execution consumes one fenced claim at a time and reports every durable checkpoint through the existing actor contract.
- Connection loss and ambiguous MOVE/COPY outcomes reconcile before replay. Offline changes survive restart and converge after reconnection.
- Representative large-mailbox and multi-hour synchronization soaks remain within the memory contract.

## M5: drafts, outbox, and SMTP

Status: pending.

Acceptance criteria:

- Drafts persist locally and survive restart. Compose validation never reports success before durable acceptance.
- MIME output streams to a private file and a bounded SQLite outbox is the source of truth for delivery attempts.
- SMTP submission supports cancellation, bounded retry/backoff, authentication recovery, permanent failure, and user-visible delivery state.
- A successful UI result means the message was durably queued or explicitly delivered; no simulated send path remains.

## M6: multi-account scheduling and optional JMAP

Status: pending.

Acceptance criteria:

- One bounded scheduler fairly serves all accounts without a runtime, worker pool, or timer per account.
- Connection, download, parsing, and provider-work concurrency have measured global and per-account caps.
- Optional JMAP uses bounded property selection and `/changes`; unused HTTP, JSON, and WebSocket features stay outside the default build.
- IMAP and JMAP share the same durable desired-state and report semantics, with provider compatibility fixtures.

## M7: release hardening

Status: pending.

Acceptance criteria:

- CI covers formatting, tests, Clippy, release builds, feature-tree policy, GUI smoke, and supported platform targets.
- Responsive breakpoints, focus containment/restoration, screen-reader semantics, high contrast, reduced motion, font scaling, and minimum target sizes are verified.
- Idle, large mailbox, repeated search/open/close, attachment, offline/reconnect, send retry, and multi-account synchronization workloads run for release-representative durations.
- Supported reference environments stay below 90 MiB idle RSS, target 50 MiB where documented, and remain below 2x settled growth.
- Packages, upgrades, rollback behavior, diagnostics, privacy documentation, GPLv3 notices, and corresponding-source materials are ready for distribution.
