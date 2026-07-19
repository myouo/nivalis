#!/usr/bin/env bash

set -euo pipefail

data_dir=${1:-}
if [[ -z "$data_dir" || "$data_dir" != /* ]]; then
    printf 'Usage: %s /absolute/isolated/data-directory\n' "$0" >&2
    exit 1
fi

database="$data_dir/mail.sqlite3"
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
fixture="$script_dir/fixtures/memory.sql"

if ! command -v sqlite3 >/dev/null 2>&1; then
    printf 'sqlite3 is required to seed the memory fixture\n' >&2
    exit 1
fi
if [[ ! -f "$database" ]]; then
    printf 'Initialize the database with Nivalis before seeding: %s\n' "$database" >&2
    exit 1
fi

schema_version=$(sqlite3 "$database" 'PRAGMA user_version;')
if [[ "$schema_version" != "8" ]]; then
    printf 'Expected schema version 8, found %s\n' "$schema_version" >&2
    exit 1
fi

account_count=$(sqlite3 "$database" 'SELECT count(*) FROM accounts;')
if [[ "$account_count" != "0" ]]; then
    printf 'Refusing to seed a database that already contains accounts\n' >&2
    exit 1
fi

chmod 700 "$data_dir"
chmod 600 "$database"
sqlite3 "$database" <"$fixture" >/dev/null
chmod 600 "$database"

integrity=$(sqlite3 "$database" 'PRAGMA integrity_check;')
foreign_key_violations=$(sqlite3 "$database" 'PRAGMA foreign_key_check;')
counts=$(sqlite3 -separator '|' "$database" \
    'SELECT (SELECT count(*) FROM accounts),
            (SELECT count(*) FROM folders),
            (SELECT count(*) FROM messages),
            (SELECT count(*) FROM message_content),
            (SELECT count(*) FROM account_mailbox_stats WHERE dirty);')
bounds=$(sqlite3 -separator '|' "$database" \
    'SELECT max(length(CAST(preview AS BLOB))),
            max(length(CAST(reader_excerpt AS BLOB)))
     FROM messages
     JOIN message_content ON message_content.message_id = messages.id;')

if [[ "$integrity" != "ok" || -n "$foreign_key_violations" ]]; then
    printf 'Seeded memory fixture failed SQLite integrity checks\n' >&2
    exit 1
fi
if [[ "$counts" != "64|64|51|51|0" || "$bounds" != "2048|65536" ]]; then
    printf 'Seeded memory fixture did not preserve its resource bounds\n' >&2
    exit 1
fi

printf 'Seeded %s (%s; preview/detail bounds %s bytes)\n' "$database" "$counts" "$bounds"
