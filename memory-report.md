# Memory Report

## Contract

Nivalis uses the following Linux release acceptance criteria:

- Default idle resident set size (RSS) below 90MiB.
- Stretch target: default idle RSS below 50MiB at the tested viewport.
- Settled RSS, PSS, RSS+Swap, and PSS+SwapPss after bounded work below 2x their pre-workload baselines.
- Idle CPU returns to 0% over a 10-second interval after startup or stress settles.

RSS includes every resident shared page mapped by the process. PSS divides shared pages by their current number of mappers. USS is `Private_Clean + Private_Dirty`. Swap and SwapPss expose pages displaced by host memory pressure so a falling resident set cannot hide retained allocation. The benchmark records these values from `/proc/<pid>/smaps_rollup`, retains the largest sampled RSS or reported VmHWM, and rejects a changed process start identity.

These numbers are machine- and viewport-specific. Software framebuffer memory grows with physical pixel area, and PSS varies with the set of concurrently running processes.

## Configuration

- Measurement date and host: 2026-07-19, Linux 7.1.2-zen3-1-zen x86_64, Rust 1.96.1.
- Release-code revision: `a74b8bb95e17c801d5aa72dde33bf84f69718cce`, schema v9.
- Production build: `cargo build --locked --release`, stripped, `opt-level = "s"`, 19,432,824 bytes (18.53MiB), SHA-256 `13f56756e327013b9901f3d78646ccc370f891c623f1f1cc5b22bfd324784fb4`.
- Benchmark build: `cargo build --locked --release --features bench-harness`, stripped, 19,457,016 bytes (18.56MiB), SHA-256 `5495ad55799ae46238371ca1aae02f396d1b26a1012376de17a7114d02bc1ab4`.
- UI state: light theme, three-pane inbox, 64 accounts, one account warning, 51 inbox messages, and bounded 50-row plus one-row pages connected through First/Next/Previous keyset navigation.
- Data bounds: all 51 stored previews are exactly 2,048 bytes and all 51 reader excerpts are exactly 65,536 bytes. The 3.7MiB private fixture passes SQLite, foreign-key, and FTS integrity checks; the production query returns 50 rows plus a cursor, then one row without another cursor, while `message 51` has exactly one FTS hit at row 51.
- Backend state: active bounded Tokio core and single-connection SQLite actor, WAL mode, 1MiB page cache limit, persistent statistics, and real account/mailbox/detail projections.
- Default renderer: `winit` + `skia-software` (Skia CPU rasterization and partial rendering).
- GPU override: `NIVALIS_RENDERER=skia`.
- X11 viewport: 1200x900 physical pixels, scale factor 1.
- Native Wayland baseline: default 1200x800 logical window, forced scale factor 1 for repeatability.
- Sampling: fresh process, `/proc/<pid>/smaps_rollup`, process-identity and interval CPU data from `/proc/<pid>/stat`; the harness verifies X11 geometry, exact stress completion, a five-second quiet grace, and a separate ten-second zero-CPU window. The 14-column CSV includes RSS, PSS, USS, Anonymous, Swap, SwapPss, reported VmHWM, and a cross-sample resident peak.

Committed samples use one CSV per measured code revision. The `test_case` column identifies each workload and repeat without multiplying evidence files:

- [`docs/measurements/2026-07-19-a74b8bb.csv`](docs/measurements/2026-07-19-a74b8bb.csv), SHA-256 `58dd1f44a27f9a186e25a82c9fbd6bda0d63d80b12a1ef916f216f3728e8cdb9`; [completion log](docs/measurements/2026-07-19-a74b8bb.log), SHA-256 `115a6f1b129385713602b6d0b101d9c94c40cabda2b8f06b293c00f8588d0554`.
- Older `0d3453c` and `d19cec5` files remain committed as schema-v8 historical evidence. They do not contain swap columns and are not attributed to the current binary.

## Idle Results

Values below are the worst stable samples across the stated fresh-process runs.

| Renderer | Platform | Runs | RSS | PSS | USS | Result |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| Skia software, `a74b8bb` schema-v9 fixture | X11 | 3 | 37.75MiB | 24.34MiB | 20.75MiB | Hard gate and tested target pass |
| Skia software, `0d3453c` bounded fixture | X11 | 3 | 37.51MiB | 24.63MiB | 21.23MiB | Hard gate pass; repeated target |
| Skia software, `d19cec5` bounded fixture | X11 | 3 + 300s soak | 37.60MiB | 24.60MiB | 20.77MiB | Historical local-read gate |
| Skia software, retained outlier | X11 | 1 historical | 68.62MiB | Not retained | Not retained | Hard gate pass; target fail |
| Skia software | X11 | 3 historical | 37.80MiB | 24.02MiB | 20.78MiB | Pre-controller-cutover pass |
| Skia software | Wayland | 3 historical | 41.5MiB | 22.4MiB | 17.5MiB | Pre-SQLite reference |
| Skia OpenGL | X11 | 1 historical | 248.0MiB | 81.9MiB | 34.0MiB | Pre-SQLite reference; RSS stretch fail |

The `a74b8bb` matrix uses three fresh production processes sampled at 5, 10, 20, and 30 seconds, followed by the quiet grace and dedicated 10-second CPU sample at 45 seconds. Their largest RSS values were 38,264, 38,316, and 38,660KiB; the matrix maxima were 24,921KiB PSS and 21,252KiB USS, with zero Swap. All three final CPU intervals were 0.00%. This exact matrix meets both the 90MiB hard gate and preferred 50MiB target, but the target remains conditional because of the retained historical outlier below.

During investigation of the preceding `73d1fdb` artifact, one populated-cache soak reported 70,264KiB (68.62MiB) RSS through 120 seconds before dropping to 61,684KiB at 300 seconds. It stayed below the hard gate but exceeded the preferred target. The value did not reproduce in the next four scripted fresh-process runs, two manual fully visible runs, the repeated five-minute soak, or the exact-binary matrix above. It remains an unexplained RSS outlier rather than being discarded; PSS and USS were not retained, so its ownership cannot be inferred. The Wayland and OpenGL rows remain historical and require refresh before use as current backend gates.

## Growth Results

The baseline and settled columns show `RSS/PSS + Swap/SwapPss` in KiB. Growth shows resident `RSS/PSS`, followed by swap-inclusive `RSS+Swap/PSS+SwapPss`.

| Scenario | Baseline | Settled | Growth | Peak RSS | Result |
| --- | --- | --- | --- | ---: | --- |
| 1,000 write/search cycles, repeat 1 | 38,200/24,528 + 0/0 | 35,780/22,002 + 11,024/2,904 | -6.34%/-10.30%; +22.52%/+1.54% | 39,100 | Pass |
| 1,000 write/search cycles, repeat 2 | 38,100/24,578 + 7,612/0 | 8,404/688 + 32,428/9,380 | -77.94%/-97.20%; -10.68%/-59.04% | 38,100 | Pass |
| 10,000 keyset transitions, repeat 1 | 33,948/21,579 + 11,992/0 | 17,784/7,452 + 22,968/6,112 | -47.61%/-65.47%; -11.29%/-37.14% | 33,948 | Pass |
| 10,000 keyset transitions, repeat 2 | 34,088/23,138 + 11,968/0 | 23,552/10,276 + 20,632/4,948 | -30.91%/-55.59%; -4.06%/-34.20% | 34,088 | Pass |

Each write/search run completed exactly 1,000 star transactions, 1,000 deterministic one-hit FTS queries, 1,000 clears, and 3,000 authoritative First queries. They took 372.577 and 375.187 seconds, or 2.684 and 2.665 cycles per second. Each pagination run completed exactly 5,000 `After` and 5,000 `Before` transitions, returned to page one, and took 153.117 and 146.391 seconds, or 65.310 and 68.310 transitions per second. All four logs contain one completion marker and no error marker; every dedicated post-workload CPU interval is 0.00%.

The largest resident peak was 39,100KiB and the largest sampled `RSS+Swap` was 47,244KiB. Across the complete idle and stress matrix, the worst settled ratios were 1.000x RSS, 1.00004x PSS, 1.22524x `RSS+Swap`, and 1.01542x `PSS+SwapPss`, all below the 2x gate. Significant host swapping occurred, so the lower settled resident values are not claimed as deallocation or an optimization gain; the swap-inclusive totals determine the growth conclusion.

This gate covers bounded controller writes, one deterministic FTS result, clear-search refreshes, and exact two-page navigation. It does not cover Archive, Trash, undo, permanent deletion, large FTS rebuilds or result sets, CJK tokenization, large-mailbox deep paging, protocols, MIME, attachments, or multi-account synchronization. Those paths receive fresh workload gates in their owning milestones. Older controller, resize, Wayland, and OpenGL measurements remain in the committed historical CSV files and are not attributed to `a74b8bb`.

## Historical Release Profile A/B

An earlier 2,000-step X11 workload compared three Rust optimization levels. These rows explain the retained `s` default but are not schema-v8 release-gate measurements.

| Profile | Optimization | Executable | Stress RSS | Timed event completion |
| --- | --- | ---: | ---: | ---: |
| `release-size` | `z` | 17.0MiB | 35.9MiB | Still active after 17s; settled before 20s |
| `release` | `s` | 18.0MiB | 40.2MiB | 9.41s median |
| `performance` | `3` | 21.0MiB | 42.1MiB | 9.17s median |

The `performance` profile remains available when the extra 2.5% measured active throughput matters more than roughly 2MiB of stress working set. Allocator replacement, native-only CPU flags, accessibility removal, and platform-specific backend forks were rejected because the measured benefit did not justify their footprint, compatibility, or accessibility cost.

## Reproduce

Check out the measured revision before running this procedure. The workflow requires `sqlite3`; X11 measurement also requires `xdotool` for window discovery and geometry control. Preserve the production binary before the benchmark build replaces Cargo's release output, and verify both exact binaries:

```bash
set -euo pipefail
revision=a74b8bb95e17c801d5aa72dde33bf84f69718cce
[[ $(git rev-parse HEAD) == "$revision" ]]
work=$(mktemp -d /tmp/nivalis-memory-a74b8bb.XXXXXX)

cargo build --locked --release
install -m 755 target/release/nivalis-mail "$work/nivalis-mail-production"
production_bytes=$(stat -c %s "$work/nivalis-mail-production")
production_sha256=$(sha256sum "$work/nivalis-mail-production" | cut -d' ' -f1)
[[ $production_bytes == 19432824 ]]
[[ $production_sha256 == 13f56756e327013b9901f3d78646ccc370f891c623f1f1cc5b22bfd324784fb4 ]]

cargo build --locked --release --features bench-harness
install -m 755 target/release/nivalis-mail "$work/nivalis-mail-bench"
bench_bytes=$(stat -c %s "$work/nivalis-mail-bench")
bench_sha256=$(sha256sum "$work/nivalis-mail-bench" | cut -d' ' -f1)
[[ $bench_bytes == 19457016 ]]
[[ $bench_sha256 == 5495ad55799ae46238371ca1aae02f396d1b26a1012376de17a7114d02bc1ab4 ]]
```

Initialize one schema-checked base fixture with the production binary, then leave it unopened. Create one copy for idle and one copy for each stress repeat before measuring anything:

```bash
fixture_base="$work/fixture-base"
mkdir -p "$fixture_base"
NIVALIS_MEMORY_DATA_DIR="$fixture_base" NIVALIS_MEMORY_SAMPLES="1" \
  scripts/measure-memory.sh "$work/nivalis-mail-production" \
  > "$work/fixture-init.csv"
scripts/seed-memory-fixture.sh "$fixture_base" | tee "$work/fixture-seed.log"

for case_id in idle write-search-1 write-search-2 pagination-1 pagination-2; do
  cp -a "$fixture_base" "$work/$case_id"
done
```

Run three fresh production processes against the dedicated idle copy:

```bash
NIVALIS_MEMORY_DATA_DIR="$work/idle" NIVALIS_MEMORY_TEST_CASE=idle \
NIVALIS_MEMORY_RUNS=3 \
NIVALIS_MEMORY_SAMPLES="5 10 20 30" NIVALIS_MEMORY_HARD_GATE=1 \
NIVALIS_MEMORY_HARD_CAP_KIB=92160 NIVALIS_MEMORY_GROWTH_LIMIT_PERCENT=100 \
NIVALIS_MEMORY_LOG="$work/idle.log" \
  scripts/measure-memory.sh "$work/nivalis-mail-production" \
  > "$work/idle.csv"
```

The 5- and 10-second samples establish each pre-workload baseline before the explicit 15-second delay. Run the star-write/single-hit-FTS/clear workload twice against its independent fixture copies:

```bash
for run in 1 2; do
  case_id="write-search-$run"
  NIVALIS_MEMORY_DATA_DIR="$work/$case_id" \
  NIVALIS_MEMORY_TEST_CASE="$case_id" \
  NIVALIS_STRESS_SCENARIO=write-search NIVALIS_STRESS_STEPS=1000 \
  NIVALIS_STRESS_DELAY_MS=15000 NIVALIS_STRESS_INTERVAL_MS=2 \
  NIVALIS_STRESS_TRANSITION_TIMEOUT_MS=5000 \
  NIVALIS_MEMORY_LOG="$work/${case_id}.log" \
  NIVALIS_MEMORY_SAMPLES="5 10 60 120 240 360 420 480 600 720" \
  NIVALIS_MEMORY_HARD_GATE=1 NIVALIS_MEMORY_HARD_CAP_KIB=92160 \
  NIVALIS_MEMORY_GROWTH_LIMIT_PERCENT=100 \
    scripts/measure-memory.sh "$work/nivalis-mail-bench" \
    > "$work/${case_id}.csv"
done
```

Run the exact-count bidirectional pagination workload twice:

```bash
for run in 1 2; do
  case_id="pagination-$run"
  NIVALIS_MEMORY_DATA_DIR="$work/$case_id" \
  NIVALIS_MEMORY_TEST_CASE="$case_id" \
  NIVALIS_STRESS_SCENARIO=pagination NIVALIS_STRESS_STEPS=10000 \
  NIVALIS_STRESS_DELAY_MS=15000 NIVALIS_STRESS_INTERVAL_MS=2 \
  NIVALIS_STRESS_TRANSITION_TIMEOUT_MS=5000 \
  NIVALIS_MEMORY_LOG="$work/${case_id}.log" \
  NIVALIS_MEMORY_SAMPLES="5 10 60 120 180 240 300 360" \
  NIVALIS_MEMORY_HARD_GATE=1 NIVALIS_MEMORY_HARD_CAP_KIB=92160 \
  NIVALIS_MEMORY_GROWTH_LIMIT_PERCENT=100 \
    scripts/measure-memory.sh "$work/nivalis-mail-bench" \
    > "$work/${case_id}.csv"
done
```

Verify one result marker and no error marker in every stress log, then build one CSV and one completion log for the measured revision. Timing and memory samples naturally vary on a rerun, so recompute their hashes rather than expecting the committed evidence hashes:

```bash
awk 'FNR == 1 && NR != 1 { next } { print }' \
  "$work/idle.csv" \
  "$work/write-search-1.csv" \
  "$work/write-search-2.csv" \
  "$work/pagination-1.csv" \
  "$work/pagination-2.csv" \
  > "$work/2026-07-19-a74b8bb.csv"

for case_id in write-search-1 write-search-2 pagination-1 pagination-2; do
  [[ $(grep -c '^NIVALIS_STRESS_RESULT ' "$work/${case_id}.log") == 1 ]]
  ! grep -q '^NIVALIS_STRESS_ERROR ' "$work/${case_id}.log"
done

{
  printf 'release_code_revision=%s\n' "$revision"
  printf 'production_bytes=%s\nproduction_sha256=%s\n' \
    "$production_bytes" "$production_sha256"
  printf 'bench_bytes=%s\nbench_sha256=%s\n' "$bench_bytes" "$bench_sha256"
  printf '%s\n' 'fixture=64|64|51|51|0 preview_detail=2048|2048|65536|65536 pages=50|1 ids=51..2|1 fts=51|1|51|51'
  for case_id in write-search-1 write-search-2 pagination-1 pagination-2; do
    grep '^NIVALIS_STRESS_RESULT ' "$work/${case_id}.log"
  done
} > "$work/2026-07-19-a74b8bb.log"

sha256sum "$work/2026-07-19-a74b8bb.csv" \
  "$work/2026-07-19-a74b8bb.log"
```

The hard gate also enforces the default five-second quiet grace and separate ten-second CPU settle interval. The script creates and removes an isolated private data directory unless `NIVALIS_MEMORY_DATA_DIR` is set to an absolute persistent path. Measurement CSV is written to standard output and should be redirected as above. Set `NIVALIS_MEMORY_LOG` to retain application output; otherwise the temporary log is removed.

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
- The M1 gate covers the SQLite-controller projection path at its account, page, preview, reader-excerpt, star-write, deterministic FTS, clear-search, bidirectional keyset, and exact query-count bounds. It does not exercise deep pages in a large mailbox, large FTS rebuilds or result sets, CJK tokenization, Archive/Trash/undo/permanent-delete soaks, protocols, MIME, attachment transfers, multi-account synchronization, or provider payloads. Each newly activated path requires an appropriate fresh soak before release.
- A production IMAP/JMAP adapter must keep the page boundary, store message bodies and attachments on disk, and bound rendered quoted history. Loading arbitrary multi-megabyte bodies into one text paragraph cannot satisfy a fixed process-memory ceiling.
