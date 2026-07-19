# Memory Report

## Contract

The reference Linux release must satisfy all of these gates:

- Warm-idle RSS is below 90MiB; 50MiB is the preferred target at 1200x900.
- Settled RSS, PSS, RSS+Swap, and PSS+SwapPss after bounded work are each below 2x their pre-workload baseline.
- CPU returns to 0.00% over a dedicated ten-second settled interval.

The harness samples `/proc/<pid>/smaps_rollup` and `/proc/<pid>/stat`, verifies process identity and X11 geometry, and retains both sampled RSS and the largest observed resident peak. Swap is reported because host pressure can make RSS fall without releasing allocations.

## Current Checkpoint

- Date and host: 2026-07-20, Linux 7.1.2-zen3-1-zen x86_64, Rust 1.96.1.
- Release-code revision: `d5a6c43ce7f5096cbb46052d1a477e0cc1db4063`, SQLite schema v11.
- Renderer and viewport: Winit + `skia-software`, X11, 1200x900 physical pixels, scale factor 1.
- Production build: 21,964,248 bytes, SHA-256 `c428ef05c5c7a343a6912e02e623eafce0cd2193d727610af3e7cf8f24633887`.
- Benchmark build: 22,026,712 bytes, SHA-256 `68150b801d106f672d945d80842c003c0454175541eecd1afc7cdb7ec73e4943`.
- Evidence: [`docs/measurements/2026-07-20-d5a6c43.csv`](docs/measurements/2026-07-20-d5a6c43.csv), SHA-256 `e8032093bd66cea2ed689a39119ecfe1d8569e661bd24cc99924634d5ea39864`.

The single 21-row CSV uses `test_case` to hold three production idle runs and one benchmark receive run. No runtime log, certificate, secret, key, or second evidence file is committed for this hash.

## Results

| Test case | Runs | Peak RSS | Worst PSS | Worst USS | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| Empty-account production idle | 3 | 32,556KiB (31.79MiB) | 21,588KiB | 18,672KiB | 90MiB gate and 50MiB target pass |
| One-message receive lifecycle | 1 | 37,024KiB (36.16MiB) | 24,023KiB | 19,812KiB | 90MiB gate, 50MiB target, growth, and CPU pass |

The receive action starts after the 5- and 10-second baselines. Through the production UI and core it:

1. Creates and diagnoses one app-password account using the real Linux Secret Service and platform-verified loopback TLS.
2. Opens a second IMAP session, examines INBOX, fetches one envelope and one 296-byte MIME literal, then stages and imports it through the SQLite generation and UIDVALIDITY fences.
3. Opens the resulting body stream, closes it by leaving the reader, removes the account and keyring entry, and reports exactly `imported=1 opened=1 closed=1 removed=1` in 559ms.
4. Holds the process through 120 seconds and records the dedicated settled CPU sample at 135 seconds.

| Measure | 10s baseline | 135s settled | Growth | Gate |
| --- | ---: | ---: | ---: | --- |
| RSS | 32,860KiB | 37,024KiB | +12.67% | pass |
| PSS | 20,503KiB | 24,015KiB | +17.13% | pass |
| RSS+Swap | 45,964KiB | 50,120KiB | +9.04% | pass |
| PSS+SwapPss | 20,503KiB | 24,015KiB | +17.13% | pass |

Final CPU is 0.00%. Swap ranged from 12,500 to 13,104KiB in the receive run and was also present during idle, so lower resident values are not claimed as a deallocation or optimization gain. All resident and swap-inclusive measures remain well below the 2x limit.

Account removal deliberately leaves file deletion to the delayed janitor. Immediately after the measured run SQLite contained one queued zero-reference file and no account, connection, message, content, attachment, or staging row. A separate normal production restart drained its one bounded startup batch, reduced `file_gc` to zero, removed the content file, and passed SQLite integrity and foreign-key checks. The committed end-to-end test independently imports, opens, closes, purges, and collects both a body and attachment.

This closes the scoped M4 receive-memory gate. It does not prove real-provider behavior, automatic or sparse-UID paging, folders beyond INBOX, UIDVALIDITY rebuild, repeated reconnect/IDLE, large messages, large mailboxes, multiple accounts, outbound provider writes, SMTP, or multi-hour growth. The retained historical 68.62MiB outlier also prevents an unconditional 50MiB guarantee outside the documented matrix.

## Evidence Layout

`docs/measurements` contains at most one CSV for each measured short commit hash. Every CSV has a `test_case` field; runtime completion logs are not committed. Current milestone gates are:

| Revision | Milestone | Evidence |
| --- | --- | --- |
| `d5a6c43` | M4 bounded receive | `2026-07-20-d5a6c43.csv` |
| `528c2b4` | M3 accounts and diagnostic | `2026-07-20-528c2b4.csv` |
| `8c005c8` | M2 content lifecycle | `2026-07-19-8c005c8.csv` |
| `a74b8bb` | M1 SQLite UI cutover | `2026-07-19-a74b8bb.csv` |

The other CSVs are retained investigation checkpoints, not current gates. Their original revision contains the matching historical procedure.

## Reproduce

Check out the measured revision. The procedure needs `sqlite3`, `openssl`, `xdotool`, and an unlocked Linux Secret Service session.

```bash
set -euo pipefail
revision=d5a6c43ce7f5096cbb46052d1a477e0cc1db4063
[[ $(git rev-parse HEAD) == "$revision" ]]
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
```

Measure three fresh production idle processes:

```bash
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
