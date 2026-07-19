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
- Measurement revision: `f639c4b`, schema v8; application runtime last changed at `1051596` and the measurement harness changed at `f639c4b`.
- Production build: stripped `cargo build --release`, `opt-level = "s"`, 19,506,296 bytes (18.60MiB), SHA-256 `5bb8f0470de1b19f5945b179ee57404c8f195c6b1a17240705cd50688b3b1240`.
- Interaction build: stripped `cargo build --release --features bench-harness`, 19,513,336 bytes, SHA-256 `892370e1885215dbc932b7596680fb1280b453593bac700cb094fe56bb17b1bf`.
- UI state: light theme, three-pane inbox, ten local demo messages.
- Backend state: active bounded Tokio core and single-connection SQLite actor, isolated empty private database, WAL mode, 1MiB page cache limit.
- Default renderer: `winit` + `skia-software` (Skia CPU rasterization and partial rendering).
- GPU override: `NIVALIS_RENDERER=skia`.
- X11 viewport: 1200x900 physical pixels, scale factor 1.
- Native Wayland baseline: default 1200x800 logical window, forced scale factor 1 for repeatability.
- Sampling: fresh process, `/proc/<pid>/smaps_rollup`, interval CPU from `/proc/<pid>/stat`; the harness verifies the requested X11 geometry and fails if interaction stress does not finish.

## Idle Results

Values below are the worst stable samples across the stated fresh-process runs.

| Renderer | Platform | Runs | RSS | PSS | USS | Result |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| Skia software | X11 | 3 | 37.80MiB | 24.02MiB | 20.78MiB | Current schema-v8 pass |
| Skia software | Wayland | 3 historical | 41.5MiB | 22.4MiB | 17.5MiB | Pre-SQLite reference |
| Skia OpenGL | X11 | 1 historical | 248.0MiB | 81.9MiB | 34.0MiB | Pre-SQLite reference; RSS stretch fail |

The current row is the worst stable sample from three fresh production processes at 5, 10, 20, and 30 seconds. Their stable RSS values were 37.80, 36.99, and 37.16MiB. RSS was unchanged from 10 through 30 seconds in every run, and interval CPU was 0.00% after startup. This passes both the 90MiB release gate and the 50MiB stretch target on the reference configuration.

One additional production process was sampled at 30, 60, 120, 180, and 300 seconds. RSS stayed exactly 38,512KiB (37.61MiB) from 30 through 300 seconds; PSS moved from 24,557 to 24,566KiB (+0.04%), USS from 21,248 to 21,264KiB (+0.08%), and anonymous memory stayed at 8,176KiB. Interval CPU was 0.00% from 60 seconds onward. The Wayland and OpenGL rows remain historical references and must be refreshed before they are used as current backend gates.

## Growth Results

| Scenario | Baseline RSS/PSS/USS | Settled RSS/PSS/USS | Growth RSS/PSS/USS | Result |
| --- | --- | --- | --- | --- |
| 10,000 high-frequency UI actions, X11, 300s | 37.61/23.90/20.67MiB | 42.10/28.39/25.16MiB | 11.93%/18.80%/21.75% | Current schema-v8 pass |
| Resize 1200x900 to 2560x1440 and restore, X11 | 34.8/20.1/16.3MiB | 44.8/25.0/16.1MiB | 28.9%/24.0%/-1.2% | Historical pass |
| Resize 1200x900 to 3840x2400 and restore, X11 | 37.34/23.75/20.55MiB | 67.48/38.84/20.58MiB | 80.71%/63.54%/0.11% | Current schema-v8 growth pass |
| Native Wayland maximize and restore | 38.5/20.0/15.5MiB | 64.2/33.0/15.9MiB | 66.7%/65.3%/2.6% | Historical pass |

The deterministic interaction run repeatedly selected and starred messages, opened and destroyed settings/account/composer components, issued debounced searches and guarded sync requests, and briefly loaded a 64KiB compose body. It completed 10,000 steps in 44.239 seconds. RSS remained exactly 43,112KiB from 90 through 300 seconds and interval CPU returned to 0.00%, so the post-workload working set both stayed below 50MiB and stabilized far below the 100% growth limit.

The 3840x2400 production resize began at 5 seconds and restored to 1200x900 at 10 seconds. At 60 seconds, RSS remained below the 90MiB hard gate and below 2x baseline, but exceeded the 50MiB idle stretch target. PSS grew less than RSS while USS changed by only 0.11%, which is consistent with retained shared surface mappings rather than equivalent private-heap growth. The 2560x1440 and Wayland rows are historical UI-only references.

## Historical Release Profile A/B

An earlier 2,000-step X11 workload compared three Rust optimization levels. These rows explain the retained `s` default but are not schema-v8 release-gate measurements.

| Profile | Optimization | Executable | Stress RSS | Timed event completion |
| --- | --- | ---: | ---: | ---: |
| `release-size` | `z` | 17.0MiB | 35.9MiB | Still active after 17s; settled before 20s |
| `release` | `s` | 18.0MiB | 40.2MiB | 9.41s median |
| `performance` | `3` | 21.0MiB | 42.1MiB | 9.17s median |

The `performance` profile remains available when the extra 2.5% measured active throughput matters more than roughly 2MiB of stress working set. Allocator replacement, native-only CPU flags, accessibility removal, and platform-specific backend forks were rejected because the measured benefit did not justify their footprint, compatibility, or accessibility cost.

## Reproduce

Build and run the default idle benchmark:

```bash
cargo build --release
scripts/measure-memory.sh target/release/nivalis-mail
```

Run three fresh X11 processes with the release-gate sample points:

```bash
NIVALIS_MEMORY_RUNS=3 NIVALIS_MEMORY_SAMPLES="5 10 20 30" \
  scripts/measure-memory.sh target/release/nivalis-mail
```

Run the five-minute pure-idle soak:

```bash
NIVALIS_MEMORY_SAMPLES="30 60 120 180 300" \
  scripts/measure-memory.sh target/release/nivalis-mail
```

Run the 10,000-step interaction scenario and retain its completion log:

```bash
cargo build --release --features bench-harness
NIVALIS_STRESS_STEPS=10000 \
NIVALIS_MEMORY_LOG=/tmp/nivalis-memory-stress.log \
NIVALIS_MEMORY_SAMPLES="3 6 15 30 45 60 90 120 180 300" \
  scripts/measure-memory.sh target/release/nivalis-mail
```

Run the production 3840x2400 resize and restore scenario:

```bash
cargo build --release
NIVALIS_RESIZE_STRESS_WIDTH=3840 \
NIVALIS_RESIZE_STRESS_HEIGHT=2400 \
NIVALIS_RESIZE_STRESS_AT=5 \
NIVALIS_RESIZE_STRESS_DURATION=5 \
NIVALIS_MEMORY_SAMPLES="3 6 9 12 20 30 60" \
  scripts/measure-memory.sh target/release/nivalis-mail
```

The script creates and removes an isolated private data directory unless `NIVALIS_MEMORY_DATA_DIR` is set to an absolute persistent path. Set `NIVALIS_MEMORY_LOG` to retain application output; otherwise the temporary log is removed.

## Implementation Notes

- Slint officially supports selecting the `winit-skia-software` renderer while retaining Skia: <https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_winit/>.
- Linux PSS/RSS definitions come from the kernel procfs documentation: <https://docs.kernel.org/filesystems/proc.html>.
- `ListView` virtualizes instantiated rows, while the additional 50-summary page cap bounds the backing UI model: <https://docs.slint.dev/latest/docs/slint/reference/std-widgets/views/listview/>.
- Page rows, totals, navigation counts, and account unread counts are produced in one Store pass. Stable presentation text uses shared handles, and only count changes update account rows.
- The production binary excludes the benchmark timers. Local cache content renders on the first normal frame; the loading state remains available for real asynchronous I/O.
- Core-to-UI mailbox and reader projections use independent latest-value slots. The 128-slot event channel contains only lightweight control values and at most one notification per projection class.
- SQLite mailbox replies retain one 50-row page plus persistent counters and at most 64 per-account unread values. Statistic rebuilds aggregate in SQLite and do not materialize mailbox-wide Rust collections.
- The measured database directory was mode `0700`; SQLite, WAL, and shared-memory files were mode `0600`. Thread inspection showed `nivalis-core` and `nivalis-sqlite` without an additional reply-bridge thread.
- A 280-character list preview and a 16,384-character reader shaping boundary prevent a malformed single-line body from multiplying text layout work. The full reader body remains available through explicit progressive loading.
- The current gate covers UI/component churn plus the constructed core and SQLite actor. It does not exercise future IMAP/JMAP sessions, MIME parsing, attachment transfers, multi-account synchronization, or provider payloads. Those paths require a separate representative sync soak before a provider-enabled release can inherit this result.
- A production IMAP/JMAP adapter must keep the page boundary, store message bodies and attachments on disk, and bound rendered quoted history. Loading arbitrary multi-megabyte bodies into one text paragraph cannot satisfy a fixed process-memory ceiling.
