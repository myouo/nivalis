# Memory Report

## Contract

The reference Linux release must satisfy all of these gates:

- Warm-idle RSS is below 90MiB; 50MiB is the preferred target at 1200x900.
- Settled RSS, PSS, RSS+Swap, and PSS+SwapPss after bounded work are each below 2x their pre-workload baseline.
- CPU returns to 0.00% over a dedicated ten-second settled interval.

The harness samples `/proc/<pid>/smaps_rollup` and `/proc/<pid>/stat`, verifies process identity and X11 geometry, and retains both sampled RSS and the largest observed resident peak. Swap is reported because host pressure can make RSS fall without releasing allocations.

## Current Checkpoint

- Date and host: 2026-07-20, Linux 7.1.2-zen3-1-zen x86_64, Rust 1.96.1.
- Release-code revision: `b852f10fffc8bb5352a36e0af0be18a66ff8e13c`, SQLite schema v12.
- Renderer and viewport: Winit + `skia-software`, X11, 1200x900 physical pixels, scale factor 1.
- Production build: 22,442,328 bytes, SHA-256 `39e1b511bef56c0446ad1776a77b84cf119917368dd9dbea81aabfbc6490ab61`.
- Evidence: [`docs/measurements/2026-07-20-b852f10.csv`](docs/measurements/2026-07-20-b852f10.csv), SHA-256 `09ddd3a6b69ccfaacdf27a10edbcb8d424dce054fbcc2106b85befe577c32a4c`.

The single 18-row CSV uses `test_case` for three production idle runs. No runtime log, certificate, secret, key, or second evidence file is committed for this hash. Commit `326ee3d` changes only CI policy after the measured code revision and produces the same release source tree.

## Results

| Test case | Runs | Peak RSS | Worst PSS | Worst USS | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| M5 empty-account production idle | 3 | 37,864KiB (36.98MiB) | 28,137KiB | 25,092KiB | 90MiB gate and 50MiB target pass |

Each run samples 5, 10, 20, 30, and 45 seconds, then uses a separate ten-second CPU window after a five-second grace period for the 60-second settled row. The largest baseline-to-settled change is +0.04% PSS and +0.03% RSS; all RSS/PSS and swap-inclusive values stay far below 2x. Every settled CPU value is 0.00%, and every swap value is zero.

This proves that the production binary remains below the idle contract after schema v12, the custom outbound MIME writer, Lettre SMTP transport, Rustls, the global outbox drainer, and the compose controller became active. It does not exercise a credential load, SMTP connection, DATA transfer, retry, uncertain result, or terminal-state recovery, so it does not close the M5 post-workload growth gate.

The prior M4 evidence at `d5a6c43` remains the latest measured receive workload: its loopback add/diagnose/receive/import/open/close/remove lifecycle peaked and settled at 37,024KiB RSS, grew 12.67% RSS and 17.13% PSS, and returned to 0.00% CPU. Real providers, automatic paging, multiple accounts, loopback sending, and multi-hour protocol soaks remain outside the combined proven matrix. The retained historical 68.62MiB outlier also prevents an unconditional 50MiB guarantee beyond the documented workloads.

## Evidence Layout

`docs/measurements` contains at most one CSV for each measured short commit hash. Every CSV has a `test_case` field; runtime completion logs are not committed. Current milestone gates are:

| Revision | Milestone | Evidence |
| --- | --- | --- |
| `b852f10` | M5 SMTP-enabled production idle | `2026-07-20-b852f10.csv` |
| `d5a6c43` | M4 bounded receive | `2026-07-20-d5a6c43.csv` |
| `528c2b4` | M3 accounts and diagnostic | `2026-07-20-528c2b4.csv` |
| `8c005c8` | M2 content lifecycle | `2026-07-19-8c005c8.csv` |
| `a74b8bb` | M1 SQLite UI cutover | `2026-07-19-a74b8bb.csv` |

The other CSVs are retained investigation checkpoints, not current gates. Their original revision contains the matching historical procedure.

## Reproduce

The current M5 idle checkpoint needs `xdotool` and an X11 session:

```bash
set -euo pipefail
revision=b852f10fffc8bb5352a36e0af0be18a66ff8e13c
[[ $(git rev-parse HEAD) == "$revision" ]]
work=$(mktemp -d /tmp/nivalis-memory-b852f10.XXXXXX)

cargo build --locked --release
install -m 755 target/release/nivalis-mail "$work/nivalis-mail-production"
[[ $(stat -c %s "$work/nivalis-mail-production") == 22442328 ]]
[[ $(sha256sum "$work/nivalis-mail-production" | cut -d' ' -f1) == \
  39e1b511bef56c0446ad1776a77b84cf119917368dd9dbea81aabfbc6490ab61 ]]

mkdir -p "$work/idle"
NIVALIS_MEMORY_DATA_DIR="$work/idle" NIVALIS_MEMORY_TEST_CASE=m5-idle \
NIVALIS_MEMORY_RUNS=3 NIVALIS_MEMORY_SAMPLES="5 10 20 30 45" \
NIVALIS_MEMORY_HARD_GATE=1 NIVALIS_MEMORY_HARD_CAP_KIB=92160 \
NIVALIS_MEMORY_GROWTH_LIMIT_PERCENT=100 \
NIVALIS_MEMORY_CPU_SETTLE_GATE=1 \
NIVALIS_MEMORY_CPU_SETTLE_GRACE_SECONDS=5 \
NIVALIS_MEMORY_CPU_SETTLE_SECONDS=10 \
NIVALIS_MEMORY_CPU_SETTLE_MAX_PERCENT=0.00 \
NIVALIS_MEMORY_LOG="$work/idle.log" \
  scripts/measure-memory.sh "$work/nivalis-mail-production" > "$work/idle.csv"
[[ $(sha256sum "$work/idle.csv" | cut -d' ' -f1) == \
  09ddd3a6b69ccfaacdf27a10edbcb8d424dce054fbcc2106b85befe577c32a4c ]]
```

### Prior M4 Receive Gate

The retained receive procedure belongs to `d5a6c43`; it additionally needs `sqlite3`, `openssl`, and an unlocked Linux Secret Service session:

```bash
git checkout d5a6c43ce7f5096cbb46052d1a477e0cc1db4063
work=$(mktemp -d /tmp/nivalis-memory-d5a6c43.XXXXXX)

cargo build --locked --release
install -m 755 target/release/nivalis-mail "$work/nivalis-mail-production"
[[ $(stat -c %s "$work/nivalis-mail-production") == 21964248 ]]
[[ $(sha256sum "$work/nivalis-mail-production" | cut -d' ' -f1) == \
  c428ef05c5c7a343a6912e02e623eafce0cd2193d727610af3e7cf8f24633887 ]]

cargo build --locked --release --features bench-harness
install -m 755 target/release/nivalis-mail "$work/nivalis-mail-bench"
[[ $(stat -c %s "$work/nivalis-mail-bench") == 22026712 ]]
[[ $(sha256sum "$work/nivalis-mail-bench" | cut -d' ' -f1) == \
  68150b801d106f672d945d80842c003c0454175541eecd1afc7cdb7ec73e4943 ]]

mkdir -p "$work/idle"
NIVALIS_MEMORY_DATA_DIR="$work/idle" NIVALIS_MEMORY_TEST_CASE=m4-idle \
NIVALIS_MEMORY_RUNS=3 NIVALIS_MEMORY_SAMPLES="5 10 20 30" \
NIVALIS_MEMORY_HARD_GATE=1 NIVALIS_MEMORY_HARD_CAP_KIB=92160 \
NIVALIS_MEMORY_GROWTH_LIMIT_PERCENT=100 NIVALIS_MEMORY_LOG="$work/idle.log" \
  scripts/measure-memory.sh "$work/nivalis-mail-production" > "$work/idle.csv"
```

Create a no-newline fake secret, one small message, and a one-day localhost CA. The benchmark rejects non-loopback hosts and secret files that are empty, over 16KiB, non-UTF-8, non-absolute, or accessible to group/other users.

```bash
mkdir -p "$work/account-receive"
printf '%s' 'not-a-real-password' > "$work/loopback.secret"
chmod 600 "$work/loopback.secret"
printf '%b' 'From: Memory Sender <memory@localhost>\r\nTo: Reader <reader@localhost>\r\nSubject: Received memory fixture\r\nDate: Mon, 20 Jul 2026 12:00:00 +0800\r\nMessage-ID: <memory-1@localhost>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\nBounded receive body.\r\n' > "$work/message.eml"

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=Nivalis M4 Memory CA' \
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
```

Start a two-connection TLS fixture. The first connection answers diagnostic LOGIN/CAPABILITY/EXAMINE; the second answers receive LOGIN/EXAMINE, one metadata fetch, one body fetch, and LOGOUT.

```bash
message_bytes=$(stat -c %s "$work/message.eml")
(
  printf '%b' '* OK diagnostic ready\r\nA0001 OK authenticated\r\n* CAPABILITY IMAP4rev1 IDLE\r\nA0002 OK capabilities complete\r\n* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n* 0 EXISTS\r\n* 0 RECENT\r\nA0003 OK [READ-ONLY] examined\r\n* BYE logout\r\nA0004 OK logout complete\r\n' | \
    openssl s_server -quiet -naccept 1 -accept 19997 \
      -cert "$work/server.crt" -key "$work/server.key"
  {
    printf '%b' '* OK receive ready\r\nA0001 OK [CAPABILITY IMAP4rev1] authenticated\r\n* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n* 1 EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY 777] epoch\r\n* OK [UIDNEXT 2] next\r\nA0002 OK [READ-ONLY] examined\r\n'
    printf '* 1 FETCH (UID 1 FLAGS () INTERNALDATE "20-Jul-2026 12:00:00 +0000" RFC822.SIZE %s ENVELOPE ("20 Jul 2026 12:00:00 +0000" "Received memory fixture" (("Memory Sender" NIL "memory" "localhost")) NIL NIL NIL NIL NIL NIL "<memory-1@localhost>"))\r\n' "$message_bytes"
    printf '%b' 'A0003 OK metadata fetched\r\n'
    printf '* 1 FETCH (UID 1 BODY[] {%s}\r\n' "$message_bytes"
    cat "$work/message.eml"
    printf '%b' ')\r\nA0004 OK body fetched\r\n* BYE logout\r\nA0005 OK logout complete\r\n'
  } | openssl s_server -quiet -naccept 1 -accept 19997 \
      -cert "$work/server.crt" -key "$work/server.key"
) > "$work/server.log" 2>&1 &
server_pid=$!
```

Run the release receive lifecycle and retain the process through its settled sample:

```bash
SSL_CERT_FILE="$work/ca.crt" \
NIVALIS_MEMORY_DATA_DIR="$work/account-receive" \
NIVALIS_MEMORY_TEST_CASE=m4-account-receive \
NIVALIS_STRESS_SCENARIO=account-receive NIVALIS_STRESS_STEPS=1 \
NIVALIS_STRESS_DELAY_MS=15000 NIVALIS_STRESS_INTERVAL_MS=25 \
NIVALIS_STRESS_TRANSITION_TIMEOUT_MS=45000 \
NIVALIS_STRESS_ACCOUNT_NAME='Memory receive' \
NIVALIS_STRESS_ACCOUNT_ADDRESS=memory@localhost \
NIVALIS_STRESS_ACCOUNT_LOGIN=memory@localhost \
NIVALIS_STRESS_ACCOUNT_IMAP_HOST=localhost \
NIVALIS_STRESS_ACCOUNT_IMAP_PORT=19997 \
NIVALIS_STRESS_ACCOUNT_SECRET_FILE="$work/loopback.secret" \
NIVALIS_STRESS_ACCOUNT_EXPECTED_RESULT=ready \
NIVALIS_MEMORY_SAMPLES="5 10 20 60 120" \
NIVALIS_MEMORY_HARD_GATE=1 NIVALIS_MEMORY_HARD_CAP_KIB=92160 \
NIVALIS_MEMORY_GROWTH_LIMIT_PERCENT=100 \
NIVALIS_MEMORY_CPU_SETTLE_GATE=1 \
NIVALIS_MEMORY_CPU_SETTLE_GRACE_SECONDS=5 \
NIVALIS_MEMORY_CPU_SETTLE_SECONDS=10 \
NIVALIS_MEMORY_CPU_SETTLE_MAX_PERCENT=0.00 \
NIVALIS_MEMORY_LOG="$work/account-receive.log" \
  scripts/measure-memory.sh "$work/nivalis-mail-bench" \
  > "$work/account-receive.csv"
wait "$server_pid"
```

Require the exact success marker, database consistency, and a subsequent bounded startup-janitor pass. Then combine both test cases into the only evidence file for the hash.

```bash
[[ $(grep -Ec '^NIVALIS_STRESS_RESULT scenario=account-receive steps=1 imported=1 opened=1 closed=1 removed=1 elapsed_ms=(0|[1-9][0-9]*)$' "$work/account-receive.log") == 1 ]]
! grep -q '^NIVALIS_STRESS_ERROR ' "$work/account-receive.log"
[[ $(sqlite3 "$work/account-receive/mail.sqlite3" 'SELECT count(*) FROM accounts;') == 0 ]]
[[ $(sqlite3 "$work/account-receive/mail.sqlite3" 'PRAGMA integrity_check;') == ok ]]
[[ -z $(sqlite3 "$work/account-receive/mail.sqlite3" 'PRAGMA foreign_key_check;') ]]

NIVALIS_MEMORY_DATA_DIR="$work/account-receive" \
NIVALIS_MEMORY_TEST_CASE=m4-delayed-janitor NIVALIS_MEMORY_SAMPLES=5 \
  scripts/measure-memory.sh "$work/nivalis-mail-production" \
  > "$work/janitor-check.csv"
[[ $(sqlite3 "$work/account-receive/mail.sqlite3" 'SELECT count(*) FROM file_gc;') == 0 ]]

{ head -n 1 "$work/idle.csv"; tail -n +2 "$work/idle.csv"; \
  tail -n +2 "$work/account-receive.csv"; } \
  > "$work/2026-07-20-d5a6c43.csv"
sha256sum "$work/2026-07-20-d5a6c43.csv"
```

The elapsed time is machine-dependent; for a new run, validate the marker format rather than requiring `559`. If setup fails with `cleanup_required=1`, preserve the data directory and use the production removal path to delete the test keyring item before discarding the fixture.
