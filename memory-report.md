# Memory Report

## Contract

Nivalis uses the following Linux release acceptance criteria:

- Default idle proportional set size (PSS) below 90MiB.
- Stretch target: default idle resident set size (RSS) below 50MiB at the tested viewport.
- Settled PSS and RSS after bounded interaction or maximize/restore stress below 2x their pre-stress baselines.
- Idle CPU returns to 0% over a 10-second interval after startup or stress settles.

RSS includes every resident shared page mapped by the process. PSS divides shared pages by their current number of mappers. USS is `Private_Clean + Private_Dirty`. The benchmark records all three from `/proc/<pid>/smaps_rollup`; PSS is the primary cross-process pass/fail metric.

These numbers are machine- and viewport-specific. Software framebuffer memory grows with physical pixel area, and PSS varies with the set of concurrently running processes.

## Configuration

- Build: stripped `cargo build --release`, 18.0MiB executable, `opt-level = "s"`.
- UI state: light theme, three-pane inbox, ten local demo messages.
- Default renderer: `winit` + `skia-software` (Skia CPU rasterization and partial rendering).
- GPU override: `NIVALIS_RENDERER=skia`.
- X11 viewport: 1200x900 physical pixels, scale factor 1.
- Native Wayland baseline: default 1200x800 logical window, forced scale factor 1 for repeatability.
- Sampling: fresh process, `/proc/<pid>/smaps_rollup`, interval CPU from `/proc/<pid>/stat`.

## Idle Results

Values below are the worst stable samples across the stated fresh-process runs.

| Renderer | Platform | Runs | RSS | PSS | USS | Result |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| Skia software | X11 | 3 | 35.5MiB | 21.2MiB | 18.0MiB | Pass, including stretch target |
| Skia software | Wayland | 3 | 41.5MiB | 22.4MiB | 17.5MiB | Pass, including stretch target |
| Skia OpenGL | X11 | 1 A/B run | 248.0MiB | 81.9MiB | 34.0MiB | PSS pass, RSS stretch fail |

The Skia software path is the product default because this mail UI is mostly static and partial rendering leaves measured idle CPU at 0.00%. The OpenGL renderer remains an explicit opt-in for workloads where GPU throughput is more important than process RSS.

## Growth Results

| Scenario | Baseline RSS/PSS/USS | Settled RSS/PSS/USS | Growth RSS/PSS/USS | Result |
| --- | --- | --- | --- | --- |
| 2,000 high-frequency UI actions, X11 | 35.5/21.1/17.9MiB | 40.0/25.6/22.4MiB | 12.6%/21.2%/25.0% | Pass |
| Resize 1200x900 to 2560x1440 and restore, X11 | 34.8/20.1/16.3MiB | 44.8/25.0/16.1MiB | 28.9%/24.0%/-1.2% | Pass |
| Resize 1200x900 to 3840x2400 and restore, X11 | 35.7/21.5/18.3MiB | 65.9/36.6/18.3MiB | 84.5%/70.5%/0.4% | Pass |
| Native Wayland maximize and restore | 38.5/20.0/15.5MiB | 64.2/33.0/15.9MiB | 66.7%/65.3%/2.6% | Pass |

The deterministic interaction run repeatedly selected and starred messages, opened and destroyed settings/account/composer components, issued debounced searches and guarded sync requests, and briefly loaded a 64KiB compose body. After settlement, interval CPU returned to 0.00%.

## Release Profile A/B

The same 2,000-step X11 workload was built with three Rust optimization levels. The production default uses `s`, which kept active throughput close to `3` while avoiding most of its code-working-set cost.

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

Run five fresh X11 processes:

```bash
NIVALIS_MEMORY_RUNS=5 NIVALIS_MEMORY_SAMPLES="5 10" \
  scripts/measure-memory.sh target/release/nivalis-mail
```

Run the bounded interaction scenario:

```bash
cargo build --release --features bench-harness
NIVALIS_STRESS_STEPS=2000 \
NIVALIS_MEMORY_SAMPLES="3 6 9 12 20 30" \
  scripts/measure-memory.sh target/release/nivalis-mail
```

Run the native Wayland maximize/restore scenario:

```bash
cargo build --release --features bench-harness
NIVALIS_MEMORY_PLATFORM=wayland \
NIVALIS_MAXIMIZE_STRESS=1 \
NIVALIS_MEMORY_SAMPLES="3 7 12 20" \
  scripts/measure-memory.sh target/release/nivalis-mail
```

## Implementation Notes

- Slint officially supports selecting the `winit-skia-software` renderer while retaining Skia: <https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_winit/>.
- Linux PSS/RSS definitions come from the kernel procfs documentation: <https://docs.kernel.org/filesystems/proc.html>.
- `ListView` virtualizes instantiated rows, while the additional 50-summary page cap bounds the backing UI model: <https://docs.slint.dev/latest/docs/slint/reference/std-widgets/views/listview/>.
- Page rows, totals, navigation counts, and account unread counts are produced in one Store pass. Stable presentation text uses shared handles, and only count changes update account rows.
- The production binary excludes the benchmark timers. Local cache content renders on the first normal frame; the loading state remains available for real asynchronous I/O.
- A 280-character list preview and a 16,384-character reader shaping boundary prevent a malformed single-line body from multiplying text layout work. The full reader body remains available through explicit progressive loading.
- A production IMAP/JMAP adapter must keep the page boundary, store message bodies and attachments on disk, and bound rendered quoted history. Loading arbitrary multi-megabyte bodies into one text paragraph cannot satisfy a fixed process-memory ceiling.
