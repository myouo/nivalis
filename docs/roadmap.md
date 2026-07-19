# Nivalis Mail Delivery Roadmap

## Product goal

Nivalis Mail will be a reliable, multi-account desktop mail client built with Rust, Slint, and Skia. It must receive, search, read, organize, compose, and send real mail without a WebView while keeping its release working set predictable.

The reference Linux release gate is a warm-idle RSS below 90 MiB, with 50 MiB as the target at 1200x900 using the default Skia software renderer. Settled RSS, PSS, RSS+Swap, and PSS+SwapPss after a bounded workload must remain below twice their pre-workload baselines. A milestone cannot inherit an earlier memory result when it activates a previously unmeasured dependency or execution path.

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

Status: complete.

Checkpoint: SQLite accounts, bounded bidirectional keyset pages, persistent counters, selected details, ordered local writes/undo, external-content FTS, and exact-key obsolete-query interruption drive the production UI. A file-backed vertical test covers success, empty, and repairable failure states. Release-code revision `a74b8bb` closes the memory gate with three production idle runs, two exact-count 1,000-cycle star-write/single-hit-FTS/clear soaks, and two exact-count 10,000-transition pagination soaks. The matrix peaks at 38,660KiB idle RSS and 39,100KiB workload RSS; all resident and swap-inclusive settled totals remain below 2x baseline and all dedicated CPU windows return 0.00%. The retained historical outlier still prevents an unconditional 50MiB guarantee outside this matrix.

Recorded follow-ups that do not block M1: evaluate a resumable batched FTS rebuild before large existing-database upgrades; profile a CJK-aware or trigram tokenizer before promising arbitrary substring search; add direct unit driving for every benchmark state transition; and reuse the production query entry in the fixture identity test. The current migration is atomic and the current search contract is Unicode case-folded literal phrase matching.

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

Status: complete.

Checkpoint: bounded MIME parsing writes one normalized body and at most 32 attachments through fixed buffers into private files. The SQLite actor atomically advances the message content generation, replaces all references, queues superseded files, and later rechecks and collects at most 16 orphan candidates per run. The end-to-end actor test imports, opens, closes, replaces, and collects real content. Release-code revision `8c005c8` closes the memory gate with three production idle runs and one exact-count 10,000-cycle import/open/close/collection workload: idle peaked at 38,492KiB RSS, the workload peaked at 39,684KiB, settled growth was 2.57% RSS and 3.51% PSS with zero swap, and every final CPU window returned 0.00%.

Acceptance criteria:

- MIME parsing enforces finite raw-size, header, part-count, nesting, and decoded-byte budgets before materializing bounded content.
- Bodies and attachments stream to private files without retaining whole-message payloads in the UI or SQLite actor.
- SQLite generation fencing atomically replaces a message's body and attachment references, rejecting stale writers.
- A bounded, delayed file-GC janitor rechecks SQLite references immediately before removing an orphan.
- One end-to-end test imports a message, opens and closes its bounded reader state, removes its references, and verifies orphan collection.
- The release binary is measured immediately for warm-idle memory and repeated import/open/close/collection growth against the project memory contract.

Recorded deferrals that do not block M2: persistent reservation restart recovery, lease renewal, and a strict recovery protocol move to M7; any existing reservation schema is groundwork rather than an M2 gate. Directory-capability `openat`/`unlinkat` hardening, cross-platform ACL and reparse-point handling, deep fuzzing and Miri, stronger HTML handling, and more sophisticated quoted-history extraction also move to M7. Strict commit-ambiguity handling for irreplaceable outbound data belongs to the M5 outbox contract.

## M3: accounts and security boundary

Status: pending.

Implementation checkpoint: schema v11 now stores bounded non-secret IMAP account configuration behind generation-fenced actor writes. It distinguishes disabled accounts from actionable connection failures, lets legacy cache-only accounts be configured or removed without inventing credentials, and drains accepted writes on shutdown. Removal persists `removing_credentials` before external secret deletion, advances to `removing_cache` only after confirmation, and processes at most 16 messages, 16 attachment rows, and 16 staging rows per transaction while queuing file references for the delayed janitor. A lazy capacity-eight credential actor now provides zeroized, redacted, idempotent access to the reference Linux Secret Service without plaintext fallback. The `456ad2f` release matrix passes the cold idle and 600-second content-soak gates after adding these dependencies, but does not exercise the unconnected credential worker. Account/credential coordination, restart continuation, Rustls diagnostics, OAuth flows, UI lifecycle, integration tests, and warm-path release measurement remain required before M3 can close.

Acceptance criteria:

- Users can add, diagnose, update, disable, and remove accounts without exposing secrets to SQLite or UI models.
- OAuth2 PKCE/device flows and application passwords use the operating-system keyring; access tokens remain short lived.
- Rustls and platform trust verification are mandatory. Authentication, permission, certificate, timeout, and offline failures have distinct recovery guidance.
- Account setup and connection diagnostics are cancellable, bounded, and covered by integration tests.

Recorded mainline boundary: account creation must commit its SQLite locator before storing a secret; failures remain visible and retryable. Removal coordination must resume `removing_credentials` after restart, accept only `Deleted` or `AlreadyMissing`, and never confirm locked, ambiguous, cancelled, or unavailable failures. M3 bounds message, attachment, and staging cleanup, but the final account cascade still covers folder, tombstone, and provider-state rows. Before M4 enables real synchronization, those newly active child sets must join the restart-safe bounded cleanup protocol. Rebinding a legacy cache to new credentials must also verify the same remote identity or clear/reconcile the old cache and pending intents first. Provider auto-discovery, proxy configuration, custom certificate authorities, certificate pinning, client certificates, and provider preset breadth do not block the first real account slice. Cross-platform packaging and prompt integration beyond the reference Linux credential service remain M7 release work; no platform may silently fall back to SQLite or plaintext files.

## M4: IMAP receive and synchronization

Status: pending.

Acceptance criteria:

- Capability discovery, folder discovery, UIDVALIDITY, incremental fetch, reconnect backoff, and bounded IMAP IDLE are implemented.
- Incoming state, locators, cursors, persistent statistics, legacy reconciliation, tombstones, and pending local desired dimensions merge atomically.
- Provider execution consumes one fenced claim at a time and reports every durable checkpoint through the existing actor contract.
- Connection loss and ambiguous MOVE/COPY outcomes reconcile before replay. Offline changes survive restart and converge after reconnection.
- Real provider state cannot activate until account removal covers every provider-owned child set with bounded restart recovery and legacy cache reuse verifies remote identity.
- Representative large-mailbox and multi-hour synchronization soaks remain within the memory contract.

## M5: drafts, outbox, and SMTP

Status: pending.

Acceptance criteria:

- Drafts persist locally and survive restart. Compose validation never reports success before durable acceptance.
- MIME output streams to a private file and a bounded SQLite outbox is the source of truth for delivery attempts.
- SMTP submission supports cancellation, bounded retry/backoff, authentication recovery, permanent failure, and user-visible delivery state.
- A successful UI result means the message was durably queued or explicitly delivered; no simulated send path remains.
- Irreplaceable outbound data uses a strict reservation and commit-recovery protocol so an ambiguous result cannot silently lose or duplicate a delivery attempt.

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
- Content storage hardening covers reservation recovery and lease expiry, directory-relative file operations, supported-platform ACL or reparse-point behavior, adversarial fuzzing and Miri, and the documented HTML and quoted-history policy.
- Credential hardening covers cross-process locator reservations and orphan scans, supported non-Linux stores, desktop unlock prompts and hard shutdown deadlines, core-dump/swap exposure policy, and process-level kill-point tests.
- Packages, upgrades, rollback behavior, diagnostics, privacy documentation, GPLv3 notices, and corresponding-source materials are ready for distribution.
