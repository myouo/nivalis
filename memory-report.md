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
- Release-code revision: `d19cec5`, schema v8. The SQLite controller cutover landed at `754c7db`, real-row stress at `73d1fdb`, the bounded fixture at `70db7b8`, and complete fixture-bound/page validation at `d19cec5`. Production runtime bytes are unchanged from `a85a279`.
- Production build: stripped `cargo build --release`, `opt-level = "s"`, 19,382,904 bytes (18.48MiB), SHA-256 `015e878868391f4134c9c210ab37dac8ccc05445eba39bc0f21d2c6ca403aa46`.
- Interaction build: stripped `cargo build --release --features bench-harness`, 19,390,200 bytes, SHA-256 `27aab936cd3d4adb7100657999aca8e0b4c2341e4e8349a9c83ba10de04074b6`.
- UI state: light theme, three-pane inbox, 64 accounts, one account warning, 51 inbox messages, a 50-row current page, and a present next-page cursor. The controller does not yet request that next page.
- Data bounds: all 51 stored previews are exactly 2,048 bytes and all 51 reader excerpts are exactly 65,536 bytes. The 3.7MiB private fixture passes SQLite integrity and foreign-key checks; the production query returns 50 rows plus a cursor, then the remaining row without another cursor.
- Backend state: active bounded Tokio core and single-connection SQLite actor, WAL mode, 1MiB page cache limit, persistent statistics, and real account/mailbox/detail projections.
- Default renderer: `winit` + `skia-software` (Skia CPU rasterization and partial rendering).
- GPU override: `NIVALIS_RENDERER=skia`.
- X11 viewport: 1200x900 physical pixels, scale factor 1.
- Native Wayland baseline: default 1200x800 logical window, forced scale factor 1 for repeatability.
- Sampling: fresh process, `/proc/<pid>/smaps_rollup`, interval CPU from `/proc/<pid>/stat`; the harness verifies the requested X11 geometry and fails if interaction stress does not finish.

The committed raw samples are:

- [`docs/measurements/2026-07-19-d19cec5-idle.csv`](docs/measurements/2026-07-19-d19cec5-idle.csv), SHA-256 `fa1258b33ab14cc5fc8e14a062ad904768419b03bbf61b6262598293151a31b6`.
- [`docs/measurements/2026-07-19-d19cec5-soak.csv`](docs/measurements/2026-07-19-d19cec5-soak.csv), SHA-256 `357be6047646ad8d7cb28f3db972354d5e04a5941f3de5f62301765665a42d13`.
- [`docs/measurements/2026-07-19-d19cec5-stress.csv`](docs/measurements/2026-07-19-d19cec5-stress.csv), SHA-256 `9448e78a5223515efff5e387c151f13762375a334b3ad95174984a6450c49c39`.
- [`docs/measurements/2026-07-19-d19cec5-stress.log`](docs/measurements/2026-07-19-d19cec5-stress.log), SHA-256 `e21b3abaf0f23dd31858eec45775e79de00a93cc95c514ddab65b89a8460e677`.
- [`docs/measurements/2026-07-19-d19cec5-stress-extended.csv`](docs/measurements/2026-07-19-d19cec5-stress-extended.csv), SHA-256 `091354d232a8a402eda8640b65de11d5222f2223083f9b228301d1bb317de327`.
- [`docs/measurements/2026-07-19-d19cec5-stress-extended.log`](docs/measurements/2026-07-19-d19cec5-stress-extended.log), SHA-256 `4ab02b819967a1232a4d8d10cf47af3a9e09a2ffee592b7315c0793389946158`.
- [`docs/measurements/2026-07-19-d19cec5-resize.csv`](docs/measurements/2026-07-19-d19cec5-resize.csv), SHA-256 `b67bd886ead5a147a08cdbf3d42d0973503e5fe37fe7bda616f13ffad9c16533`.

## Idle Results

Values below are the worst stable samples across the stated fresh-process runs.

| Renderer | Platform | Runs | RSS | PSS | USS | Result |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| Skia software, bounded fixture | X11 | 3 + 300s soak | 37.60MiB | 24.60MiB | 20.77MiB | Hard gate pass; repeated target |
| Skia software, retained outlier | X11 | 1 historical | 68.62MiB | Not retained | Not retained | Hard gate pass; target fail |
| Skia software | X11 | 3 historical | 37.80MiB | 24.02MiB | 20.78MiB | Pre-controller-cutover pass |
| Skia software | Wayland | 3 historical | 41.5MiB | 22.4MiB | 17.5MiB | Pre-SQLite reference |
| Skia OpenGL | X11 | 1 historical | 248.0MiB | 81.9MiB | 34.0MiB | Pre-SQLite reference; RSS stretch fail |

The current matrix uses three fresh production processes sampled at 5, 10, 20, and 30 seconds. Their stable RSS values were 37.24, 37.60, and 37.34MiB; interval CPU was 0.00% from 10 seconds onward. A separate exact-revision process held RSS exactly at 38,316KiB (37.42MiB) from 30 through 300 seconds; PSS/USS ended at 25,142/21,256KiB, anonymous memory stayed at 8,172KiB, and interval CPU was 0.00% from 60 seconds onward. All repeated exact-binary runs therefore meet the preferred 50MiB target with the maximum account catalog, a full visible page, maximum previews, and available maximum excerpts. The hard 90MiB release gate is proved; the target is not an unconditional guarantee because of the retained outlier below.

During investigation of the preceding `73d1fdb` artifact, one populated-cache soak reported 70,264KiB (68.62MiB) RSS through 120 seconds before dropping to 61,684KiB at 300 seconds. It stayed below the hard gate but exceeded the preferred target. The value did not reproduce in the next four scripted fresh-process runs, two manual fully visible runs, the repeated five-minute soak, or the exact-binary matrix above. It remains an unexplained RSS outlier rather than being discarded; PSS and USS were not retained, so its ownership cannot be inferred. The Wayland and OpenGL rows remain historical and require refresh before use as current backend gates.

## Growth Results

| Scenario | Baseline RSS/PSS/USS | Settled RSS/PSS/USS | Growth RSS/PSS/USS | Result |
| --- | --- | --- | --- | --- |
| 10,000 bounded UI/SQLite actions, X11, 300s | 37.59/24.69/20.87MiB | 45.05/32.16/28.34MiB | 19.85%/30.22%/35.77% | Hard/growth pass |
| Resize 1200x900 to 3840x2400 and restore, populated X11 | 37.46/24.50/20.71MiB | 68.53/40.52/21.66MiB | 82.91%/65.36%/4.60% | Hard/growth pass |
| 10,000 former demo-controller actions, X11, 300s | 37.61/23.90/20.67MiB | 42.10/28.39/25.16MiB | 11.93%/18.80%/21.75% | Historical pre-cutover pass |
| Native Wayland maximize and restore | 38.5/20.0/15.5MiB | 64.2/33.0/15.9MiB | 66.7%/65.3%/2.6% | Historical pass |

The current deterministic run cycles real folder queries, conditional overlays, and valid rows from the live Slint model; visible rows repeatedly load the bounded 64KiB SQLite reader detail. It also exercises search input and debounce-timer churn, but it does not count executed SQLite searches and therefore cannot prove coverage of a non-empty query. That path remains outside this gate until the M1 FTS workload adds explicit execution instrumentation.

The first exact-binary run completed 10,000 steps in 111.063 seconds. RSS remained 45,768KiB from 120 through 150 seconds, and the explicit 120-130, 130-140, and 140-150 second intervals each measured 0.00% CPU. The 150-180 second interval later averaged 8.96% and RSS rose by 320KiB, so that short run alone is not treated as settled evidence.

The exact-binary extended repeat completed in 110.502 seconds. RSS was exactly 46,136KiB from 90 through 300 seconds. CPU was 0.00% for the 120-130, 130-140, 140-150, 150-180, 180-190, 190-200, 200-210, 210-240, 240-270, and 270-300 second intervals. Its final values and growth determine the table result; the earlier late-activity observation remains in the committed short-run samples rather than being discarded.

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
cargo build --release
fixture_dir=$(mktemp -d /tmp/nivalis-memory.XXXXXX)
NIVALIS_MEMORY_DATA_DIR="$fixture_dir" NIVALIS_MEMORY_SAMPLES="1" \
  scripts/measure-memory.sh target/release/nivalis-mail \
  > /tmp/nivalis-memory-init.csv
scripts/seed-memory-fixture.sh "$fixture_dir"
```

Run three fresh X11 processes with the release-gate sample points:

```bash
NIVALIS_MEMORY_DATA_DIR="$fixture_dir" NIVALIS_MEMORY_RUNS=3 \
NIVALIS_MEMORY_SAMPLES="5 10 20 30" \
  scripts/measure-memory.sh target/release/nivalis-mail \
  > /tmp/nivalis-memory-idle.csv
```

Run the five-minute pure-idle soak:

```bash
NIVALIS_MEMORY_DATA_DIR="$fixture_dir" \
NIVALIS_MEMORY_SAMPLES="30 60 120 180 300" \
  scripts/measure-memory.sh target/release/nivalis-mail \
  > /tmp/nivalis-memory-soak.csv
```

Run the 10,000-step interaction scenario and retain its completion log:

```bash
cargo build --release --features bench-harness
NIVALIS_MEMORY_DATA_DIR="$fixture_dir" NIVALIS_STRESS_STEPS=10000 \
NIVALIS_MEMORY_LOG=/tmp/nivalis-memory-stress.log \
NIVALIS_MEMORY_SAMPLES="3 6 15 30 60 90 120 130 140 150 180 190 200 210 240 270 300" \
  scripts/measure-memory.sh target/release/nivalis-mail \
  > /tmp/nivalis-memory-stress.csv
```

Run the production 3840x2400 resize and restore scenario:

```bash
cargo build --release
NIVALIS_MEMORY_DATA_DIR="$fixture_dir" NIVALIS_RESIZE_STRESS_WIDTH=3840 \
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
- The current local-read gate covers the SQLite-controller projection path at its account, page, preview, and reader-excerpt bounds. It does not exercise IMAP/JMAP sessions, MIME parsing, attachment transfers, multi-account synchronization, controller mutations, pagination, FTS, or provider payloads. Each newly activated path requires an appropriate fresh soak before release.
- A production IMAP/JMAP adapter must keep the page boundary, store message bodies and attachments on disk, and bound rendered quoted history. Loading arbitrary multi-megabyte bodies into one text paragraph cannot satisfy a fixed process-memory ceiling.
