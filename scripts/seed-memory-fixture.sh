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
if [[ "$schema_version" != "11" ]]; then
    printf 'Expected schema version 11, found %s\n' "$schema_version" >&2
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
    'SELECT min(length(CAST(preview AS BLOB))),
            max(length(CAST(preview AS BLOB))),
            min(length(CAST(reader_excerpt AS BLOB))),
            max(length(CAST(reader_excerpt AS BLOB)))
     FROM messages
     JOIN message_content ON message_content.message_id = messages.id;')
projection=$(sqlite3 -separator '|' "$database" \
    "WITH ordered AS (
         SELECT m.id
           FROM messages AS m
          WHERE EXISTS (
                    SELECT 1
                      FROM message_folders AS mf
                      JOIN folders AS f
                        ON f.id = mf.folder_id AND f.account_id = mf.account_id
                     WHERE mf.message_id = m.id
                       AND mf.account_id = m.account_id
                       AND f.role = 'inbox'
                )
          ORDER BY m.received_at_ms DESC, m.id DESC
     )
     SELECT (SELECT count(*) FROM (SELECT id FROM ordered LIMIT 50)),
            (SELECT count(*) FROM (SELECT id FROM ordered LIMIT 1 OFFSET 50)),
            (SELECT id FROM ordered LIMIT 1),
            (SELECT id FROM ordered LIMIT 1 OFFSET 49),
            (SELECT id FROM ordered LIMIT 1 OFFSET 50);")
search=$(sqlite3 -separator '|' "$database" \
    "SELECT (SELECT count(*)
               FROM message_search
              WHERE message_search MATCH '\"bounded mailbox\"'),
            (SELECT count(*)
               FROM message_search
              WHERE message_search MATCH '\"message 51\"'),
            (SELECT min(rowid)
               FROM message_search
              WHERE message_search MATCH '\"message 51\"'),
            (SELECT max(rowid)
               FROM message_search
              WHERE message_search MATCH '\"message 51\"');")
sqlite3 "$database" \
    "INSERT INTO message_search(message_search, rank) VALUES ('integrity-check', 1);"

if [[ "$integrity" != "ok" || -n "$foreign_key_violations" ]]; then
    printf 'Seeded memory fixture failed SQLite integrity checks\n' >&2
    exit 1
fi
if [[ "$counts" != "64|64|51|51|0" || "$bounds" != "2048|2048|65536|65536" ||
    "$projection" != "50|1|51|2|1" || "$search" != "51|1|51|51" ]]; then
    printf 'Seeded memory fixture did not preserve its resource bounds\n' >&2
    exit 1
fi

IFS='|' read -r first_count second_count first_id first_last_id second_id <<<"$projection"
printf 'Seeded %s (%s; preview/detail min/max %s bytes; pages %s|%s; first IDs %s..%s; second ID %s; FTS %s)\n' \
    "$database" "$counts" "$bounds" "$first_count" "$second_count" \
    "$first_id" "$first_last_id" "$second_id" "$search"
