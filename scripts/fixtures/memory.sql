PRAGMA foreign_keys = ON;
PRAGMA recursive_triggers = ON;
BEGIN IMMEDIATE;

WITH RECURSIVE sequence(id) AS (
    VALUES (1)
    UNION ALL
    SELECT id + 1 FROM sequence WHERE id < 64
)
INSERT INTO accounts (
    id,
    provider,
    remote_key,
    name,
    address,
    sort_order,
    state,
    accent_rgb
)
SELECT
    id,
    'imap',
    'account-' || id,
    'Account ' || id || ' ' || replace(printf('%300s', ''), ' ', 'n'),
    'account-' || id || '@example.test',
    id,
    CASE WHEN id = 64 THEN 'auth_required' ELSE 'active' END,
    (id * 200000) % 16777216
FROM sequence;

WITH RECURSIVE sequence(id) AS (
    VALUES (1)
    UNION ALL
    SELECT id + 1 FROM sequence WHERE id < 64
)
INSERT INTO folders (id, account_id, remote_key, name, role)
SELECT id, id, 'inbox', 'Inbox', 'inbox'
FROM sequence;

WITH RECURSIVE sequence(id) AS (
    VALUES (1)
    UNION ALL
    SELECT id + 1 FROM sequence WHERE id < 51
)
INSERT INTO messages (
    id,
    account_id,
    remote_key,
    sender_name,
    sender_address,
    subject,
    preview,
    received_at_ms,
    unread,
    starred,
    has_attachment
)
SELECT
    id,
    ((id - 1) % 64) + 1,
    'message-' || id,
    'Sender ' || id,
    'sender-' || id || '@example.test',
    'Bounded mailbox message ' || id || ' ' || replace(printf('%400s', ''), ' ', 's'),
    replace(printf('%2048s', ''), ' ', 'p'),
    1700000000000 + id,
    id % 2,
    CASE WHEN id % 3 = 0 THEN 1 ELSE 0 END,
    CASE WHEN id % 10 = 0 THEN 1 ELSE 0 END
FROM sequence;

INSERT INTO message_folders (message_id, folder_id, account_id)
SELECT id, account_id, account_id
FROM messages;

INSERT INTO message_content (
    message_id,
    reader_excerpt,
    truncated,
    body_byte_count,
    body_file_key
)
SELECT
    id,
    replace(printf('%65536s', ''), ' ', 'b'),
    1,
    1048576,
    'body-' || id || '.mime'
FROM messages;

UPDATE account_mailbox_stats AS stats
SET inbox_total = (
        SELECT count(*)
        FROM messages
        WHERE account_id = stats.account_id
    ),
    inbox_unread = (
        SELECT count(*)
        FROM messages
        WHERE account_id = stats.account_id AND unread = 1
    ),
    starred_total = (
        SELECT count(*)
        FROM messages
        WHERE account_id = stats.account_id AND starred = 1
    ),
    sent_total = 0,
    drafts_total = 0,
    archive_total = 0,
    trash_total = 0,
    dirty = 0;

COMMIT;
PRAGMA wal_checkpoint(TRUNCATE);
