# Memory Report

## Contract

Nivalis uses the following Linux release acceptance criteria:

- Default idle resident set size (RSS) below 90MiB.
- Stretch target: default idle RSS below 50MiB at the tested viewport.
- Settled PSS and RSS after bounded interaction or maximize/restore stress below 2x their pre-stress baselines.
- Idle CPU returns to 0% over a 10-second interval after startup or stress settles.

RSS includes every resident shared page mapped by the process. PSS divides shared pages by their current number of mappers. USS is `Private_Clean + Private_Dirty`. The benchmark records all three from `/proc/<pid>/smaps_rollup`; RSS is the conservative release gate, while PSS and USS distinguish shared mappings from private growth.

These numbers are machine- and viewport-specific. Software framebuffer memory grows with physical pixel area, and PSS varies with the set of concurrently running processes.

## Configuration

- Measurement date and host: 2026-07-19, Linux 7.1.2-zen3-1-zen x86_64, Rust 1.96.1.
- Release-code revision: `0d3453c5860e1efcdb90cd9e61819dba030e5695`, schema v8. Bidirectional controller pagination landed at `2d7f26d`; exact query-count and memory-gate coverage landed at `0d3453c`.
- Production build: `cargo build --locked --release`, stripped, `opt-level = "s"`, 19,410,424 bytes (18.51MiB), SHA-256 `2f74e875620a6ca2f005c9521accf74652aa0a37b68729892180a254a434081c`.
- Pagination build: `cargo build --locked --release --features bench-harness`, stripped, 19,425,528 bytes (18.53MiB), SHA-256 `4547ace84c4d4b2895cb2eb0cb0c86484c706608563e11c65d919f43d50a69f4`.
- UI state: light theme, three-pane inbox, 64 accounts, one account warning, 51 inbox messages, and bounded 50-row plus one-row pages connected through First/Next/Previous keyset navigation.
- Data bounds: all 51 stored previews are exactly 2,048 bytes and all 51 reader excerpts are exactly 65,536 bytes. The 3.7MiB private fixture passes SQLite integrity and foreign-key checks; the production query returns 50 rows plus a cursor, then the remaining row without another cursor.
- Backend state: active bounded Tokio core and single-connection SQLite actor, WAL mode, 1MiB page cache limit, persistent statistics, and real account/mailbox/detail projections.
- Default renderer: `winit` + `skia-software` (Skia CPU rasterization and partial rendering).
- GPU override: `NIVALIS_RENDERER=skia`.
- X11 viewport: 1200x900 physical pixels, scale factor 1.
- Native Wayland baseline: default 1200x800 logical window, forced scale factor 1 for repeatability.
- Sampling: fresh process, `/proc/<pid>/smaps_rollup`, interval CPU from `/proc/<pid>/stat`; the harness verifies the requested X11 geometry and fails if interaction stress does not finish.

Committed samples use one CSV per measured code revision. The `test_case` column identifies each workload and repeat without multiplying evidence files:

- [`docs/measurements/2026-07-19-0d3453c.csv`](docs/measurements/2026-07-19-0d3453c.csv), SHA-256 `8179f3a68222ee829b002779bbf901d378ce740d9a3fb1ee18c97c297e6e9684`; [completion log](docs/measurements/2026-07-19-0d3453c.log), SHA-256 `3c198c469c098d11ac20683c4bf4525b182809a663c01d95d3e5c5575bbe1dc9`.
- [`docs/measurements/2026-07-19-d19cec5.csv`](docs/measurements/2026-07-19-d19cec5.csv), SHA-256 `e76adb4b34e5554165cde279294bcba37958db30c980ed082244a8999c6c97b2`; [completion log](docs/measurements/2026-07-19-d19cec5.log), SHA-256 `84a84b28096be3e9e89e8368006a6ab49ffeb4d641dee07421fb9725852bc385`.

## Idle Results

Values below are the worst stable samples across the stated fresh-process runs.

| Renderer | Platform | Runs | RSS | PSS | USS | Result |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| Skia software, `0d3453c` bounded fixture | X11 | 3 | 37.51MiB | 24.63MiB | 21.23MiB | Hard gate pass; repeated target |
| Skia software, `d19cec5` bounded fixture | X11 | 3 + 300s soak | 37.60MiB | 24.60MiB | 20.77MiB | Historical local-read gate |
| Skia software, retained outlier | X11 | 1 historical | 68.62MiB | Not retained | Not retained | Hard gate pass; target fail |
| Skia software | X11 | 3 historical | 37.80MiB | 24.02MiB | 20.78MiB | Pre-controller-cutover pass |
| Skia software | Wayland | 3 historical | 41.5MiB | 22.4MiB | 17.5MiB | Pre-SQLite reference |
| Skia OpenGL | X11 | 1 historical | 248.0MiB | 81.9MiB | 34.0MiB | Pre-SQLite reference; RSS stretch fail |

The `0d3453c` matrix uses three fresh production processes sampled at 5, 10, 20, and 30 seconds. Their RSS values were stable at 38,308KiB (37.41MiB), 38,408KiB (37.51MiB), and 38,156KiB (37.26MiB); the largest PSS/USS/VmHWM values were 25,220/21,740/38,408KiB. Interval CPU was 0.00% from 10 seconds onward in every run. All three exact-binary runs meet the preferred 50MiB target with the maximum account catalog, a full visible page, maximum previews, and available maximum excerpts. The hard 90MiB release gate is proved; the target is not an unconditional guarantee because of the retained outlier below. The separate five-minute `d19cec5` pure-idle soak remains historical evidence rather than being attributed to this newer binary.

During investigation of the preceding `73d1fdb` artifact, one populated-cache soak reported 70,264KiB (68.62MiB) RSS through 120 seconds before dropping to 61,684KiB at 300 seconds. It stayed below the hard gate but exceeded the preferred target. The value did not reproduce in the next four scripted fresh-process runs, two manual fully visible runs, the repeated five-minute soak, or the exact-binary matrix above. It remains an unexplained RSS outlier rather than being discarded; PSS and USS were not retained, so its ownership cannot be inferred. The Wayland and OpenGL rows remain historical and require refresh before use as current backend gates.

## Growth Results

| Scenario | Baseline RSS/PSS/USS | Settled RSS/PSS/USS | Growth RSS/PSS/USS | Result |
| --- | --- | --- | --- | --- |
| 10,000 keyset transitions, repeat 1, X11, 300s | 37.21/24.50/21.13MiB | 37.46/24.81/21.46MiB | 0.68%/1.26%/1.57% | Hard/growth pass |
| 10,000 keyset transitions, repeat 2, X11, 300s | 37.16/24.35/20.96MiB | 37.44/24.55/21.32MiB | 0.75%/0.84%/1.73% | Hard/growth pass |
| 10,000 bounded UI/SQLite actions, `d19cec5`, X11, 300s | 37.59/24.69/20.87MiB | 45.05/32.16/28.34MiB | 19.85%/30.22%/35.77% | Historical local-read pass |
| Resize 1200x900 to 3840x2400 and restore, populated X11 | 37.46/24.50/20.71MiB | 68.53/40.52/21.66MiB | 82.91%/65.36%/4.60% | Hard/growth pass |
| 10,000 former demo-controller actions, X11, 300s | 37.61/23.90/20.67MiB | 42.10/28.39/25.16MiB | 11.93%/18.80%/21.75% | Historical pre-cutover pass |
| Native Wayland maximize and restore | 38.5/20.0/15.5MiB | 64.2/33.0/15.9MiB | 66.7%/65.3%/2.6% | Historical pass |

The current pagination workload waits for the exact seeded first-page signature, records a query-counter baseline, and alternates 5,000 `After` and 5,000 `Before` keyset queries. Every reply must match the expected 50-row or one-row page before the next transition begins. Each measured run reported exactly one completion marker with `transitions=10000 after=5000 before=5000 final_page=1`; extra First queries, stale results, timeouts, counter regressions, or mismatched pages fail closed. The state machine retains one request, two counter snapshots, and no page history.

Repeat 1 completed the 10,000 transitions in 147.900 seconds after the configured 15-second delay. RSS rose from 38,104KiB at 5 seconds to a peak and final value of 38,364KiB; VmHWM was 38,364KiB. CPU was 0.00% in every measured interval from 210 through 300 seconds.

Repeat 2 completed in 147.534 seconds after the same delay. RSS rose from 38,052KiB to 38,336KiB at 300 seconds; VmHWM was 38,336KiB. CPU returned to 0.00% in every measured interval from 210 through 290 seconds. The retained 290-300 second interval measured 5.69% and RSS added 16KiB, so this report does not claim continuously zero CPU through the endpoint; it does prove multiple post-stress zero-CPU intervals and remains far inside both memory gates.

This two-page fixture proves bounded bidirectional controller navigation, exact SQLite query classes, and stable post-navigation residency. It does not prove deep-page behavior in a large mailbox, mutations, FTS, provider traffic, MIME, or attachments. Those paths require their own instrumented workloads when activated.

The historical `d19cec5` deterministic run cycles real folder queries, conditional overlays, and valid rows from the live Slint model; visible rows repeatedly load the bounded 64KiB SQLite reader detail. It also exercises search input and debounce-timer churn, but it does not count executed SQLite searches and therefore cannot prove coverage of a non-empty query. That path remains outside this gate until the M1 FTS workload adds explicit execution instrumentation.

The first historical `d19cec5` exact-binary run completed 10,000 steps in 111.063 seconds. RSS remained 45,768KiB from 120 through 150 seconds, and the explicit 120-130, 130-140, and 140-150 second intervals each measured 0.00% CPU. The 150-180 second interval later averaged 8.96% and RSS rose by 320KiB, so that short run alone is not treated as settled evidence.

The historical `d19cec5` exact-binary extended repeat completed in 110.502 seconds. RSS was exactly 46,136KiB from 90 through 300 seconds. CPU was 0.00% for the 120-130, 130-140, 140-150, 150-180, 180-190, 190-200, 200-210, 210-240, 240-270, and 270-300 second intervals. Its final values and growth determine that historical table row; the earlier late-activity observation remains in the committed short-run samples rather than being discarded.

The populated 3840x2400 production resize began at 5 seconds and restored to 1200x900 at 10 seconds. The measured high-resolution peak was 68.53/40.53/21.67MiB RSS/PSS/USS during the 6-12 second samples, or +82.91%/+65.41%/+4.66%. At 60 seconds the retained surface was 68.53/40.52/21.66MiB, so the table reports the settled values while separately exposing the almost identical transient peak. RSS stayed below the 90MiB hard gate and below 2x baseline but exceeded the 50MiB normal-idle target; the small USS change is consistent with retained surface mappings rather than equivalent private-heap growth.

## Historical Release Profile A/B

An earlier 2,000-step X11 workload compared three Rust optimization levels. These rows explain the retained `s` default but are not schema-v8 release-gate measurements.

| Profile | Optimization | Executable | Stress RSS | Timed event completion |
| --- | --- | ---: | ---: | ---: |
| `release-size` | `z` | 17.0MiB | 35.9MiB | Still active after 17s; settled before 20s |
| `release` | `s` | 18.0MiB | 40.2MiB | 9.41s median |
| `performance` | `3` | 21.0MiB | 42.1MiB | 9.17s median |

The `performance` profile remains available when the extra 2.5% measured active throughput matters more than roughly 2MiB of stress working set. Allocator replacement, native-only CPU flags, accessibility removal, and platform-specific backend forks were rejected because the measured benefit did not justify their footprint, compatibility, or accessibility cost.

## Reproduce

Build a production binary, initialize an isolated database, and seed the schema-checked bounded fixture. The workflow requires `sqlite3`; X11 measurement also requires `xdotool` for window discovery and geometry control. The seed script refuses to touch a database that already contains an account:

```bash
cargo build --locked --release
fixture_dir=$(mktemp -d /tmp/nivalis-memory.XXXXXX)
NIVALIS_MEMORY_DATA_DIR="$fixture_dir" NIVALIS_MEMORY_SAMPLES="1" \
  scripts/measure-memory.sh target/release/nivalis-mail \
  > /tmp/nivalis-memory-init.csv
scripts/seed-memory-fixture.sh "$fixture_dir"
```

Run three fresh X11 processes with the release-gate sample points:

```bash
NIVALIS_MEMORY_DATA_DIR="$fixture_dir" NIVALIS_MEMORY_TEST_CASE=idle \
NIVALIS_MEMORY_RUNS=3 \
NIVALIS_MEMORY_SAMPLES="5 10 20 30" NIVALIS_MEMORY_HARD_GATE=1 \
NIVALIS_MEMORY_HARD_CAP_KIB=92160 NIVALIS_MEMORY_GROWTH_LIMIT_PERCENT=100 \
  scripts/measure-memory.sh target/release/nivalis-mail \
  > /tmp/nivalis-memory-idle.csv
```

Optionally run a fresh five-minute pure-idle soak. The committed `0d3453c` CSV does not attribute the historical `d19cec5` soak to the newer binary, so this optional output is not part of the merge below:

```bash
NIVALIS_MEMORY_DATA_DIR="$fixture_dir" \
NIVALIS_MEMORY_TEST_CASE=idle-soak \
NIVALIS_MEMORY_SAMPLES="30 60 120 180 300" \
  scripts/measure-memory.sh target/release/nivalis-mail \
  > /tmp/nivalis-memory-soak.csv
```

Run the 10,000-transition pagination scenario twice in fresh processes and inspect each completion log before merging the CSV rows under distinct `test_case` values. The 5- and 10-second samples establish the pre-stress baseline before the explicit 15-second delay:

```bash
cargo build --locked --release --features bench-harness
NIVALIS_MEMORY_DATA_DIR="$fixture_dir" \
NIVALIS_MEMORY_TEST_CASE=pagination-1 \
NIVALIS_STRESS_SCENARIO=pagination NIVALIS_STRESS_STEPS=10000 \
NIVALIS_STRESS_DELAY_MS=15000 NIVALIS_STRESS_INTERVAL_MS=2 \
NIVALIS_STRESS_TRANSITION_TIMEOUT_MS=5000 \
NIVALIS_MEMORY_LOG=/tmp/nivalis-memory-pagination-run-1.log \
NIVALIS_MEMORY_SAMPLES="5 10 20 30 60 90 120 150 180 210 240 270 280 290 300" \
NIVALIS_MEMORY_HARD_GATE=1 NIVALIS_MEMORY_HARD_CAP_KIB=92160 \
NIVALIS_MEMORY_GROWTH_LIMIT_PERCENT=100 \
  scripts/measure-memory.sh target/release/nivalis-mail \
  > /tmp/nivalis-memory-pagination-run-1.csv

# Repeat with NIVALIS_MEMORY_TEST_CASE=pagination-2 and run-2 output paths.

awk 'FNR == 1 && NR != 1 { next } { print }' \
  /tmp/nivalis-memory-idle.csv \
  /tmp/nivalis-memory-pagination-run-1.csv \
  /tmp/nivalis-memory-pagination-run-2.csv \
  > /tmp/nivalis-memory-0d3453c.csv
```

Run the production 3840x2400 resize and restore scenario:

```bash
cargo build --locked --release
NIVALIS_MEMORY_DATA_DIR="$fixture_dir" NIVALIS_RESIZE_STRESS_WIDTH=3840 \
NIVALIS_MEMORY_TEST_CASE=resize-3840x2400 \
NIVALIS_RESIZE_STRESS_HEIGHT=2400 \
NIVALIS_RESIZE_STRESS_AT=5 \
NIVALIS_RESIZE_STRESS_DURATION=5 \
NIVALIS_MEMORY_SAMPLES="3 6 9 12 15 30 60" \
  scripts/measure-memory.sh target/release/nivalis-mail \
  > /tmp/nivalis-memory-resize.csv
```

The script creates and removes an isolated private data directory unless `NIVALIS_MEMORY_DATA_DIR` is set to an absolute persistent path. Measurement CSV is written to standard output and should be redirected as above. Set `NIVALIS_MEMORY_LOG` to retain application output; otherwise the temporary log is removed.

## Implementation Notes

- Slint officially supports selecting the `winit-skia-software` renderer while retaining Skia: <https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_winit/>.
- Linux PSS/RSS definitions come from the kernel procfs documentation: <https://docs.kernel.org/filesystems/proc.html>.
- `ListView` virtualizes instantiated rows, while the additional 50-summary page cap bounds the backing UI model: <https://docs.slint.dev/latest/docs/slint/reference/std-widgets/views/listview/>.
- Page rows, persistent totals, navigation counts, and account unread counts are produced by one bounded SQLite query and projection. Stable presentation text uses shared handles, and a bounded account-directory reply replaces at most 64 account rows.
- The production binary excludes the benchmark timers. Local cache content renders on the first normal frame; the loading state remains available for real asynchronous I/O.
- Core-to-UI mailbox and reader projections use independent latest-value slots. The 128-slot event channel contains only lightweight control values and at most one notification per projection class.
- SQLite mailbox replies retain one 50-row page plus persistent counters and at most 64 per-account unread values. Statistic rebuilds aggregate in SQLite and do not materialize mailbox-wide Rust collections.
- The measured database directory was mode `0700`; SQLite, WAL, and shared-memory files were mode `0600`. Thread inspection showed `nivalis-core` and `nivalis-sqlite` without an additional reply-bridge thread.
- A 2,048-byte stored preview and 64KiB reader-excerpt boundary prevent malformed content from multiplying text layout work. Full-body loading remains unavailable until the bounded file-content pipeline is connected.
- The current local-read and pagination gate covers the SQLite-controller projection path at its account, page, preview, reader-excerpt, bidirectional keyset, and exact query-count bounds. It does not exercise deep pages in a large mailbox, IMAP/JMAP sessions, MIME parsing, attachment transfers, multi-account synchronization, controller mutations, FTS, or provider payloads. Each newly activated path requires an appropriate fresh soak before release.
- A production IMAP/JMAP adapter must keep the page boundary, store message bodies and attachments on disk, and bound rendered quoted history. Loading arbitrary multi-megabyte bodies into one text paragraph cannot satisfy a fixed process-memory ceiling.
