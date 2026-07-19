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

- Measurement date and host: 2026-07-20, Linux 7.1.2-zen3-1-zen x86_64, Rust 1.96.1.
- Release-code revision: `528c2b440db1c731110b12e88bc9b532d1a52d16`, schema v11.
- Production build: `cargo build --locked --release`, stripped, `opt-level = "s"`, 21,439,064 bytes (20.45MiB), SHA-256 `10e5d0c41055a68b9b03d2ff5f25f2c704cab34e6fa2ddaec0771b1631212105`.
- Benchmark build: `cargo build --locked --release --features bench-harness`, stripped, 21,913,560 bytes (20.90MiB), SHA-256 `99362b362d4807435a81d604a19f2e8900014d2f3c3be27d12ea6e2b77661429`.
- UI state: light theme at 1200x900. Production idle uses the empty-account onboarding state. The protocol case transiently drives the production add, connection-status, and remove surfaces, then returns to the same empty state.
- Data state: both current fixtures start empty. The protocol case atomically creates one generation-fenced app-password account, stores and loads one fake loopback-only secret through the real Linux Secret Service, records one successful diagnostic, deletes the keyring item, purges the account, and ends with zero account, connection, staging, and GC rows. SQLite integrity and foreign-key checks pass.
- Backend state: active bounded Tokio core, single-connection SQLite actor, lazy credential actor, platform-verifying Rustls, and `async-imap`. The local TLS fixture completes greeting, LOGIN, CAPABILITY, and EXAMINE INBOX through the production diagnostic function, followed by its bounded best-effort LOGOUT attempt; no mock connector or second runtime is used.
- Default renderer: `winit` + `skia-software` (Skia CPU rasterization and partial rendering).
- GPU override: `NIVALIS_RENDERER=skia`.
- X11 viewport: 1200x900 physical pixels, scale factor 1.
- Native Wayland baseline: default 1200x800 logical window, forced scale factor 1 for repeatability.
- Sampling: fresh process, `/proc/<pid>/smaps_rollup`, process-identity and interval CPU data from `/proc/<pid>/stat`; the harness verifies X11 geometry, exact stress completion, a five-second quiet grace, and a separate ten-second zero-CPU window. The 14-column CSV includes RSS, PSS, USS, Anonymous, Swap, SwapPss, reported VmHWM, and a cross-sample resident peak.

Committed samples use one CSV per measured code revision. The `test_case` column identifies each workload and repeat without multiplying evidence files:

- [`docs/measurements/2026-07-20-528c2b4.csv`](docs/measurements/2026-07-20-528c2b4.csv), SHA-256 `0a0cbf69faf493ca829d0d630c42713e8a4c8e69acdb799fd1db2701f4cec756`. Its 21 data rows contain three production idle runs and one complete loopback account-diagnostic lifecycle; no per-hash log or second evidence file is committed.
- [`docs/measurements/2026-07-20-9f0fd17.csv`](docs/measurements/2026-07-20-9f0fd17.csv), SHA-256 `6fed4cd86ed589188df2f6fcc4c522d7f41bafd9c9e6ad164ffe7bb4325998c5`. Its 31 data rows contain the preceding cold-idle, credential-recovery, and content-soak matrix; no per-hash log or second evidence file is committed.
- [`docs/measurements/2026-07-19-456ad2f.csv`](docs/measurements/2026-07-19-456ad2f.csv), SHA-256 `1ec859a8d0689d588074b3e882d36cd243500fb55dd62ede4f76471e9b51e8df`. Its `test_case` rows contain the complete idle and 600-second content-soak matrix; no second per-hash evidence file is committed.
- [`docs/measurements/2026-07-19-8c005c8.csv`](docs/measurements/2026-07-19-8c005c8.csv), SHA-256 `5642ab4d752d1ade19b2abde8b11a633965006de90271a09954c50ae77ef5c9f`; [completion log](docs/measurements/2026-07-19-8c005c8.log), SHA-256 `213b452f45c3a8c96fc214284d086efba5d60c151511ae97438e83a71e9d4427`.
- [`docs/measurements/2026-07-19-a74b8bb.csv`](docs/measurements/2026-07-19-a74b8bb.csv), SHA-256 `58dd1f44a27f9a186e25a82c9fbd6bda0d63d80b12a1ef916f216f3728e8cdb9`; [completion log](docs/measurements/2026-07-19-a74b8bb.log), SHA-256 `115a6f1b129385713602b6d0b101d9c94c40cabda2b8f06b293c00f8588d0554`.
- The `a74b8bb` file remains the M1 SQLite-controller matrix. Older `0d3453c` and `d19cec5` files remain schema-v8 historical evidence and do not contain swap columns.

## Idle Results

Values below are the worst stable samples across the stated fresh-process runs.

| Renderer | Platform | Runs | RSS | PSS | USS | Result |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| Skia software, `528c2b4` empty-account state | X11 | 3 | 35.93MiB | 26.54MiB | 23.96MiB | Current M3 hard gate and tested target pass |
| Skia software, `9f0fd17` schema-v11 fixture | X11 | 3 | 38.07MiB | 28.67MiB | 26.11MiB | Pre-M3 populated-fixture pass |
| Skia software, `456ad2f` schema-v11 fixture | X11 | 3 | 37.79MiB | 28.14MiB | 25.52MiB | Pre-coordinator gate pass |
| Skia software, `8c005c8` schema-v10 fixture | X11 | 3 | 37.59MiB | 27.62MiB | 24.84MiB | M2 hard gate and tested target pass |
| Skia software, `a74b8bb` schema-v9 fixture | X11 | 3 | 37.75MiB | 24.34MiB | 20.75MiB | Hard gate and tested target pass |
| Skia software, `0d3453c` bounded fixture | X11 | 3 | 37.51MiB | 24.63MiB | 21.23MiB | Hard gate pass; repeated target |
| Skia software, `d19cec5` bounded fixture | X11 | 3 + 300s soak | 37.60MiB | 24.60MiB | 20.77MiB | Historical local-read gate |
| Skia software, retained outlier | X11 | 1 historical | 68.62MiB | Not retained | Not retained | Hard gate pass; target fail |
| Skia software | X11 | 3 historical | 37.80MiB | 24.02MiB | 20.78MiB | Pre-controller-cutover pass |
| Skia software | Wayland | 3 historical | 41.5MiB | 22.4MiB | 17.5MiB | Pre-SQLite reference |
| Skia OpenGL | X11 | 1 historical | 248.0MiB | 81.9MiB | 34.0MiB | Pre-SQLite reference; RSS stretch fail |

The `528c2b4` matrix uses three fresh production processes sampled at 5, 10, 20, and 30 seconds, followed by the quiet grace and dedicated 10-second CPU sample at 45 seconds. Their largest RSS values were 36,792, 36,428, and 36,612KiB; the matrix maxima were 27,176KiB PSS and 24,532KiB USS, with zero Swap. All three final CPU intervals were 0.00%. The current revision meets both the 90MiB hard gate and preferred 50MiB target at the tested viewport.

The preceding `9f0fd17` populated-fixture matrix peaked at 38,980KiB RSS, 29,356KiB PSS, and 26,736KiB USS across three fresh processes.

The preceding `456ad2f` matrix peaked at 38,700KiB RSS before the account coordinator became reachable from the core.

The `8c005c8` M2 matrix remains the preceding content baseline. Its three fresh production processes peaked at 38,492KiB RSS and also passed both thresholds.

The `a74b8bb` M1 matrix remains the preceding SQLite-controller baseline: its three production processes peaked at 38,660KiB RSS, with zero Swap and 0.00% settled CPU.

During investigation of the preceding `73d1fdb` artifact, one populated-cache soak reported 70,264KiB (68.62MiB) RSS through 120 seconds before dropping to 61,684KiB at 300 seconds. It stayed below the hard gate but exceeded the preferred target. The value did not reproduce in the next four scripted fresh-process runs, two manual fully visible runs, the repeated five-minute soak, or the exact-binary matrix above. It remains an unexplained RSS outlier rather than being discarded; PSS and USS were not retained, so its ownership cannot be inferred. The Wayland and OpenGL rows remain historical and require refresh before use as current backend gates.

## Growth Results

The baseline and settled columns show `RSS/PSS + Swap/SwapPss` in KiB. Growth shows resident `RSS/PSS`, followed by swap-inclusive `RSS+Swap/PSS+SwapPss`.

| Scenario | Baseline | Settled | Growth | Peak RSS | Result |
| --- | --- | --- | --- | ---: | --- |
| One complete account diagnostic plus 120s warm hold, `528c2b4` | 36,880/27,293 + 0/0 | 38,988/28,754 + 0/0 | +5.72%/+5.35%; +5.72%/+5.35% | 38,988 | Current M3 path pass |
| Secret Service recovery plus 600s warm idle, `9f0fd17` | 39,448/29,778 + 0/0 | 39,448/29,776 + 0/0 | 0.00%/-0.01%; 0.00%/-0.01% | 39,448 | Pre-M3 warm pass |
| 10,000 content lifecycle cycles plus 600s soak, `9f0fd17` | 39,032/29,381 + 0/0 | 40,032/30,388 + 0/0 | +2.56%/+3.43%; +2.56%/+3.43% | 40,084 | M2 content pass |
| 10,000 content lifecycle cycles plus 600s soak, `456ad2f` | 38,856/28,882 + 0/0 | 39,792/29,780 + 0/0 | +2.41%/+3.11%; +2.41%/+3.11% | 39,844 | Pre-coordinator pass |
| 10,000 content lifecycle cycles | 38,640/28,281 + 0/0 | 39,632/29,273 + 0/0 | +2.57%/+3.51%; +2.57%/+3.51% | 39,684 | M2 pass |
| 1,000 write/search cycles, repeat 1 | 38,200/24,528 + 0/0 | 35,780/22,002 + 11,024/2,904 | -6.34%/-10.30%; +22.52%/+1.54% | 39,100 | Pass |
| 1,000 write/search cycles, repeat 2 | 38,100/24,578 + 7,612/0 | 8,404/688 + 32,428/9,380 | -77.94%/-97.20%; -10.68%/-59.04% | 38,100 | Pass |
| 10,000 keyset transitions, repeat 1 | 33,948/21,579 + 11,992/0 | 17,784/7,452 + 22,968/6,112 | -47.61%/-65.47%; -11.29%/-37.14% | 33,948 | Pass |
| 10,000 keyset transitions, repeat 2 | 34,088/23,138 + 11,968/0 | 23,552/10,276 + 20,632/4,948 | -30.91%/-55.59%; -4.06%/-34.20% | 34,088 | Pass |

The `528c2b4` account case started from an empty persistent data directory. After the 5- and 10-second baselines, one 128ms lifecycle drove the production UI add/status/remove callbacks, stored and loaded one fake secret through the real Secret Service, completed platform-verified loopback TLS, LOGIN, CAPABILITY, and EXAMINE followed by best-effort LOGOUT, fenced the diagnostic report, then deleted the secret and account. RSS rose from 36,880KiB to 38,988KiB and stayed there at 20, 60, 120, and the 135-second CPU-settle sample; PSS settled at 28,754KiB, Swap remained zero, and final CPU was 0.00%. SQLite ended with no account, connection, staging, or GC rows and passed integrity and foreign-key checks.

The preceding `9f0fd17` warm case started from one generation-fenced `removing_credentials` account. Production opened the real Secret Service, converged the deliberately absent locator through `AlreadyMissing`, removed the account and connection, and left one file reference for the delayed janitor. RSS remained exactly 39,448KiB from 5 through 615 seconds; PSS changed from 29,778 to 29,776KiB, Swap remained zero, and the final CPU interval was 0.00%.

The `9f0fd17` content workload completed exactly 10,000 bounded MIME imports, 10,000 body streams, 10,000 attachment streams, and 10,000 bounded GC runs in 34.566 seconds. It examined 19,999 old file references, removed 19,998 files, converged one intentionally missing fixture file, and ended with generation 10,000, empty staging and GC tables, and only the current body and attachment on disk. RSS settled at 40,032KiB from 60 through 615 seconds, peak RSS was 40,084KiB (39.14MiB), final growth was 2.56% RSS and 3.43% PSS, Swap remained zero, and the final CPU interval was 0.00%.

The preceding `456ad2f` workload completed the same exact-count lifecycle in 32.136 seconds with a 39,844KiB peak and 2.41%/3.11% settled RSS/PSS growth.

The preceding `8c005c8` M2 workload completed the same exact-count lifecycle in 35.515 seconds with a 39,684KiB peak and 2.57%/3.51% settled RSS/PSS growth.

Each historical M1 write/search run completed exactly 1,000 star transactions, 1,000 deterministic one-hit FTS queries, 1,000 clears, and 3,000 authoritative First queries. Each pagination run completed exactly 5,000 `After` and 5,000 `Before` transitions and returned to page one. All four logs contain one completion marker and no error marker; every dedicated post-workload CPU interval is 0.00%.

Across the retained current and historical matrices, the largest non-outlier resident peak was 40,084KiB and the largest sampled `RSS+Swap` was 47,244KiB. The current M3 diagnostic settles at 1.0572x RSS and 1.0535x PSS; the worst retained ratios remain below 1.226x for all resident and swap-inclusive measures, below the 2x gate. Significant host swapping occurred during M1, so its lower settled resident values are not claimed as deallocation or an optimization gain; the swap-inclusive totals determine that conclusion.

The current checkpoint proves production empty idle and one complete loopback add/keyring-store/keyring-load/fenced-diagnostic/IMAP/remove/keyring-delete path. It does not prove warm idle with a retained configured account, public DNS or provider behavior, desktop unlock prompts, repeated diagnostics, multiple accounts, OAuth, receive synchronization, SMTP/outbox delivery, large FTS rebuilds, deep large-mailbox paging, or representative long-duration protocol activity. Each newly activated milestone requires its own release measurement rather than inheriting this result.

## Historical Release Profile A/B

An earlier 2,000-step X11 workload compared three Rust optimization levels. These rows explain the retained `s` default but are not schema-v8 release-gate measurements.

| Profile | Optimization | Executable | Stress RSS | Timed event completion |
| --- | --- | ---: | ---: | ---: |
| `release-size` | `z` | 17.0MiB | 35.9MiB | Still active after 17s; settled before 20s |
| `release` | `s` | 18.0MiB | 40.2MiB | 9.41s median |
| `performance` | `3` | 21.0MiB | 42.1MiB | 9.17s median |

The `performance` profile remains available when the extra 2.5% measured active throughput matters more than roughly 2MiB of stress working set. Allocator replacement, native-only CPU flags, accessibility removal, and platform-specific backend forks were rejected because the measured benefit did not justify their footprint, compatibility, or accessibility cost.

## Reproduce

Check out `528c2b440db1c731110b12e88bc9b532d1a52d16`. The workflow requires `sqlite3`, `openssl`, and `xdotool`, plus a usable Linux Secret Service session. Build and preserve the production binary before the benchmark build replaces Cargo's release output:

```bash
set -euo pipefail
work=$(mktemp -d /tmp/nivalis-memory-528c2b4.XXXXXX)
cargo build --locked --release
install -m 755 target/release/nivalis-mail "$work/nivalis-mail-production"
[[ $(stat -c %s "$work/nivalis-mail-production") == 21439064 ]]
[[ $(sha256sum "$work/nivalis-mail-production" | cut -d' ' -f1) == \
  10e5d0c41055a68b9b03d2ff5f25f2c704cab34e6fa2ddaec0771b1631212105 ]]
cargo build --locked --release --features bench-harness
install -m 755 target/release/nivalis-mail "$work/nivalis-mail-bench"
[[ $(stat -c %s "$work/nivalis-mail-bench") == 21913560 ]]
[[ $(sha256sum "$work/nivalis-mail-bench" | cut -d' ' -f1) == \
  99362b362d4807435a81d604a19f2e8900014d2f3c3be27d12ea6e2b77661429 ]]
```

Create independent empty data directories and a fake loopback-only secret file. The harness rejects non-loopback hosts, non-absolute secret paths, files accessible to group or other users, non-UTF-8 data, empty data, and files over 16KiB:

```bash
mkdir -p "$work/idle" "$work/account-diagnostic"
printf '%s' 'not-a-real-password' > "$work/loopback.secret"
chmod 600 "$work/loopback.secret"
```

Run the production idle matrix:

```bash
NIVALIS_MEMORY_DATA_DIR="$work/idle" NIVALIS_MEMORY_TEST_CASE=m3-idle \
NIVALIS_MEMORY_RUNS=3 NIVALIS_MEMORY_SAMPLES="5 10 20 30" \
NIVALIS_MEMORY_HARD_GATE=1 NIVALIS_MEMORY_HARD_CAP_KIB=92160 \
NIVALIS_MEMORY_GROWTH_LIMIT_PERCENT=100 NIVALIS_MEMORY_LOG="$work/idle.log" \
  scripts/measure-memory.sh "$work/nivalis-mail-production" > "$work/idle.csv"
```

Create a one-day local CA and a `localhost` server certificate, then start a single-connection scripted TLS endpoint. The CA is trusted only by this benchmark process through `SSL_CERT_FILE`:

```bash
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=Nivalis memory CA' \
  -addext 'basicConstraints=critical,CA:TRUE' \
  -addext 'keyUsage=critical,keyCertSign,cRLSign' \
  -keyout "$work/ca.key" -out "$work/ca.crt"
openssl req -newkey rsa:2048 -nodes -subj '/CN=localhost' \
  -addext 'subjectAltName=DNS:localhost' \
  -addext 'extendedKeyUsage=serverAuth' \
  -keyout "$work/server.key" -out "$work/server.csr"
openssl x509 -req -days 1 -in "$work/server.csr" \
  -CA "$work/ca.crt" -CAkey "$work/ca.key" -CAcreateserial \
  -copy_extensions copy -out "$work/server.crt"
openssl verify -CAfile "$work/ca.crt" -verify_hostname localhost "$work/server.crt"

printf '%b' \
  '* OK Nivalis memory IMAP ready\r\nA0001 OK authenticated\r\n* CAPABILITY IMAP4rev1 IDLE\r\nA0002 OK capabilities complete\r\n* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n* 0 EXISTS\r\n* 0 RECENT\r\nA0003 OK [READ-ONLY] mailbox examined\r\n* BYE logging out\r\nA0004 OK logout complete\r\n' | \
  openssl s_server -quiet -naccept 1 -accept 19993 \
    -cert "$work/server.crt" -key "$work/server.key" \
    > "$work/server.log" 2>&1 &
server_pid=$!
```

Run exactly one production account lifecycle after two baseline samples. The script accepts only a `ready` completion that removes the account, then holds the process through 120 seconds and records the dedicated settled CPU sample at 135 seconds:

```bash
SSL_CERT_FILE="$work/ca.crt" \
NIVALIS_MEMORY_DATA_DIR="$work/account-diagnostic" \
NIVALIS_MEMORY_TEST_CASE=m3-account-diagnostic-ready \
NIVALIS_STRESS_SCENARIO=account-diagnostic NIVALIS_STRESS_STEPS=1 \
NIVALIS_STRESS_DELAY_MS=15000 NIVALIS_STRESS_INTERVAL_MS=25 \
NIVALIS_STRESS_TRANSITION_TIMEOUT_MS=45000 \
NIVALIS_STRESS_ACCOUNT_NAME='Memory diagnostic' \
NIVALIS_STRESS_ACCOUNT_ADDRESS='memory@localhost' \
NIVALIS_STRESS_ACCOUNT_LOGIN='memory@localhost' \
NIVALIS_STRESS_ACCOUNT_IMAP_HOST=localhost \
NIVALIS_STRESS_ACCOUNT_IMAP_PORT=19993 \
NIVALIS_STRESS_ACCOUNT_SECRET_FILE="$work/loopback.secret" \
NIVALIS_STRESS_ACCOUNT_EXPECTED_RESULT=ready \
NIVALIS_MEMORY_SAMPLES="5 10 20 60 120" \
NIVALIS_MEMORY_HARD_GATE=1 NIVALIS_MEMORY_HARD_CAP_KIB=92160 \
NIVALIS_MEMORY_GROWTH_LIMIT_PERCENT=100 \
NIVALIS_MEMORY_CPU_SETTLE_GATE=1 \
NIVALIS_MEMORY_CPU_SETTLE_GRACE_SECONDS=5 \
NIVALIS_MEMORY_CPU_SETTLE_SECONDS=10 \
NIVALIS_MEMORY_LOG="$work/account-diagnostic.log" \
  scripts/measure-memory.sh "$work/nivalis-mail-bench" \
  > "$work/account-diagnostic.csv"
wait "$server_pid"
```

The temporary diagnostic log must contain exactly one `outcome=ready removed=1` completion marker and no error marker. Verify that SQLite has no retained account, connection, staging, or GC rows and passes both consistency checks. Combine the idle header and all data rows into one per-revision CSV; do not commit the fake secret, CA, server key, or logs. If a failed run reports `cleanup_required=1`, preserve the printed data directory until the same profile can resume account removal.

```bash
[[ $(grep -c '^NIVALIS_STRESS_RESULT scenario=account-diagnostic cycles=1 outcome=ready removed=1 ' \
  "$work/account-diagnostic.log") == 1 ]]
! grep -q '^NIVALIS_STRESS_ERROR ' "$work/account-diagnostic.log"
[[ $(sqlite3 "$work/account-diagnostic/mail.sqlite3" \
  'SELECT count(*) FROM accounts;') == 0 ]]
[[ $(sqlite3 "$work/account-diagnostic/mail.sqlite3" \
  'SELECT count(*) FROM account_connections;') == 0 ]]
[[ $(sqlite3 "$work/account-diagnostic/mail.sqlite3" \
  'SELECT count(*) FROM file_staging;') == 0 ]]
[[ $(sqlite3 "$work/account-diagnostic/mail.sqlite3" \
  'SELECT count(*) FROM file_gc;') == 0 ]]
[[ $(sqlite3 "$work/account-diagnostic/mail.sqlite3" \
  'PRAGMA integrity_check;') == ok ]]
[[ -z $(sqlite3 "$work/account-diagnostic/mail.sqlite3" \
  'PRAGMA foreign_key_check;') ]]
{ head -n 1 "$work/idle.csv"; tail -n +2 "$work/idle.csv"; \
  tail -n +2 "$work/account-diagnostic.csv"; } \
  > "$work/2026-07-20-528c2b4.csv"
```

### Historical `9f0fd17` procedure

The preceding schema-v11 credential-recovery and content-soak matrix used revision `9f0fd179d32d3a360a723a7cbadfa85862934cff`. Its full commands remain reproducible from that revision's copy of this report and are retained as historical evidence rather than the current M3 gate.

### Historical M1 procedure

The commands below reproduce the retained M1 SQLite-controller matrix rather than the current M2 gate. Check out its measured revision before running them:

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
- A 2,048-byte preview and 64KiB reader-excerpt boundary prevent malformed content from multiplying text layout work. Full bodies and attachments remain private files and are opened as bounded streams rather than copied into SQLite or mailbox-wide UI state.
- The M2 gate adds MIME/file import, generation replacement, stream-open, and orphan collection to the preceding M1 SQLite-controller coverage. It does not exercise deep pages, large FTS rebuilds, account credentials, protocols, outbound delivery, multi-account synchronization, or provider payloads. Each newly activated path requires a fresh soak in its owning milestone.
- A production IMAP/JMAP adapter must keep the page boundary, store message bodies and attachments on disk, and bound rendered quoted history. Loading arbitrary multi-megabyte bodies into one text paragraph cannot satisfy a fixed process-memory ceiling.
