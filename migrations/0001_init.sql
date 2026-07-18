CREATE TABLE accounts (
    id          INTEGER PRIMARY KEY CHECK (id > 0),
    provider    TEXT NOT NULL
                CHECK (length(CAST(provider AS BLOB)) BETWEEN 1 AND 64),
    remote_key  TEXT NOT NULL
                CHECK (length(CAST(remote_key AS BLOB)) BETWEEN 1 AND 512),
    name        TEXT NOT NULL
                CHECK (length(CAST(name AS BLOB)) BETWEEN 1 AND 320),
    address     TEXT NOT NULL
                CHECK (length(CAST(address AS BLOB)) BETWEEN 1 AND 320),
    sort_order  INTEGER NOT NULL DEFAULT 0
                CHECK (sort_order BETWEEN 0 AND 2147483647),
    state       TEXT NOT NULL DEFAULT 'active'
                CHECK (length(CAST(state AS BLOB)) BETWEEN 1 AND 64),
    accent_rgb  INTEGER NOT NULL DEFAULT 0
                CHECK (accent_rgb BETWEEN 0 AND 16777215),
    UNIQUE (provider, remote_key)
) STRICT;

CREATE TABLE folders (
    id          INTEGER PRIMARY KEY CHECK (id > 0),
    account_id  INTEGER NOT NULL
                REFERENCES accounts(id) ON DELETE CASCADE,
    remote_key  TEXT NOT NULL
                CHECK (length(CAST(remote_key AS BLOB)) BETWEEN 1 AND 512),
    name        TEXT NOT NULL
                CHECK (length(CAST(name AS BLOB)) BETWEEN 1 AND 320),
    role        TEXT NOT NULL
                CHECK (length(CAST(role AS BLOB)) BETWEEN 1 AND 64),
    UNIQUE (account_id, remote_key),
    UNIQUE (id, account_id)
) STRICT;

CREATE TABLE messages (
    id              INTEGER PRIMARY KEY CHECK (id > 0),
    account_id      INTEGER NOT NULL
                    REFERENCES accounts(id) ON DELETE CASCADE,
    remote_key      TEXT NOT NULL
                    CHECK (length(CAST(remote_key AS BLOB)) BETWEEN 1 AND 512),
    sender_name     TEXT NOT NULL DEFAULT ''
                    CHECK (length(CAST(sender_name AS BLOB)) <= 320),
    sender_address  TEXT NOT NULL DEFAULT ''
                    CHECK (length(CAST(sender_address AS BLOB)) <= 320),
    subject         TEXT NOT NULL DEFAULT ''
                    CHECK (length(CAST(subject AS BLOB)) <= 998),
    preview         TEXT NOT NULL DEFAULT ''
                    CHECK (length(CAST(preview AS BLOB)) <= 2048),
    received_at_ms  INTEGER NOT NULL
                    CHECK (received_at_ms BETWEEN -62135596800000 AND 253402300799999),
    unread          INTEGER NOT NULL DEFAULT 1
                    CHECK (unread IN (0, 1)),
    starred         INTEGER NOT NULL DEFAULT 0
                    CHECK (starred IN (0, 1)),
    has_attachment  INTEGER NOT NULL DEFAULT 0
                    CHECK (has_attachment IN (0, 1)),
    revision        INTEGER NOT NULL DEFAULT 0
                    CHECK (revision BETWEEN 0 AND 9223372036854775807),
    UNIQUE (account_id, remote_key),
    UNIQUE (id, account_id)
) STRICT;

CREATE TABLE message_folders (
    message_id  INTEGER NOT NULL,
    folder_id   INTEGER NOT NULL,
    account_id  INTEGER NOT NULL,
    PRIMARY KEY (message_id, folder_id),
    FOREIGN KEY (message_id, account_id)
        REFERENCES messages(id, account_id) ON DELETE CASCADE,
    FOREIGN KEY (folder_id, account_id)
        REFERENCES folders(id, account_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE message_content (
    message_id       INTEGER PRIMARY KEY
                     REFERENCES messages(id) ON DELETE CASCADE,
    reader_excerpt   TEXT NOT NULL DEFAULT ''
                     CHECK (length(CAST(reader_excerpt AS BLOB)) <= 65536),
    truncated        INTEGER NOT NULL DEFAULT 0
                     CHECK (truncated IN (0, 1)),
    body_byte_count  INTEGER NOT NULL DEFAULT 0
                     CHECK (body_byte_count BETWEEN 0 AND 1099511627776),
    body_file_key    TEXT
                     CHECK (
                         body_file_key IS NULL OR
                         length(CAST(body_file_key AS BLOB)) BETWEEN 1 AND 512
                     )
) STRICT;

CREATE TABLE attachments (
    id            INTEGER PRIMARY KEY CHECK (id > 0),
    message_id    INTEGER NOT NULL
                  REFERENCES messages(id) ON DELETE CASCADE,
    ordinal       INTEGER NOT NULL
                  CHECK (ordinal BETWEEN 0 AND 65535),
    remote_key    TEXT
                  CHECK (
                      remote_key IS NULL OR
                      length(CAST(remote_key AS BLOB)) BETWEEN 1 AND 512
                  ),
    file_name     TEXT NOT NULL DEFAULT ''
                  CHECK (length(CAST(file_name AS BLOB)) <= 998),
    media_type    TEXT NOT NULL DEFAULT 'application/octet-stream'
                  CHECK (length(CAST(media_type AS BLOB)) BETWEEN 1 AND 255),
    content_id    TEXT
                  CHECK (
                      content_id IS NULL OR
                      length(CAST(content_id AS BLOB)) BETWEEN 1 AND 998
                  ),
    disposition   TEXT NOT NULL DEFAULT 'attachment'
                  CHECK (length(CAST(disposition AS BLOB)) BETWEEN 1 AND 64),
    byte_count    INTEGER NOT NULL DEFAULT 0
                  CHECK (byte_count BETWEEN 0 AND 1099511627776),
    file_key      TEXT
                  CHECK (
                      file_key IS NULL OR
                      length(CAST(file_key AS BLOB)) BETWEEN 1 AND 512
                  ),
    UNIQUE (message_id, ordinal)
) STRICT;

CREATE TABLE sync_state (
    folder_id        INTEGER PRIMARY KEY
                     REFERENCES folders(id) ON DELETE CASCADE,
    uid_validity     INTEGER
                     CHECK (uid_validity IS NULL OR uid_validity BETWEEN 0 AND 4294967295),
    change_cursor    TEXT
                     CHECK (
                         change_cursor IS NULL OR
                         length(CAST(change_cursor AS BLOB)) BETWEEN 1 AND 512
                     ),
    last_sync_at_ms  INTEGER
                     CHECK (
                         last_sync_at_ms IS NULL OR
                         last_sync_at_ms BETWEEN -62135596800000 AND 253402300799999
                     ),
    last_error       TEXT
                     CHECK (
                         last_error IS NULL OR
                         length(CAST(last_error AS BLOB)) BETWEEN 1 AND 2048
                     )
) STRICT;

CREATE TABLE outbox (
    message_id           INTEGER PRIMARY KEY
                         REFERENCES messages(id) ON DELETE CASCADE,
    mime_file_key        TEXT NOT NULL
                         CHECK (length(CAST(mime_file_key AS BLOB)) BETWEEN 1 AND 512),
    envelope_from        TEXT NOT NULL
                         CHECK (length(CAST(envelope_from AS BLOB)) BETWEEN 1 AND 320),
    wire_byte_count      INTEGER NOT NULL
                         CHECK (wire_byte_count BETWEEN 1 AND 1099511627776),
    state                TEXT NOT NULL
                         CHECK (length(CAST(state AS BLOB)) BETWEEN 1 AND 64),
    attempt_count        INTEGER NOT NULL DEFAULT 0
                         CHECK (attempt_count BETWEEN 0 AND 1000),
    next_attempt_at_ms   INTEGER
                         CHECK (
                             next_attempt_at_ms IS NULL OR
                             next_attempt_at_ms BETWEEN -62135596800000 AND 253402300799999
                         ),
    error_code           TEXT
                         CHECK (
                             error_code IS NULL OR
                             length(CAST(error_code AS BLOB)) BETWEEN 1 AND 64
                         )
) STRICT;

CREATE TABLE outbox_recipients (
    message_id    INTEGER NOT NULL
                  REFERENCES outbox(message_id) ON DELETE CASCADE,
    kind          TEXT NOT NULL
                  CHECK (kind IN ('to', 'cc', 'bcc')),
    ordinal       INTEGER NOT NULL
                  CHECK (ordinal BETWEEN 0 AND 65535),
    address       TEXT NOT NULL
                  CHECK (length(CAST(address AS BLOB)) BETWEEN 1 AND 320),
    display_name  TEXT NOT NULL DEFAULT ''
                  CHECK (length(CAST(display_name AS BLOB)) <= 320),
    PRIMARY KEY (message_id, kind, ordinal)
) STRICT;

CREATE INDEX idx_folders_account
    ON folders(account_id, id);

CREATE INDEX idx_message_folders_folder
    ON message_folders(folder_id, account_id, message_id);

CREATE INDEX idx_messages_account_time
    ON messages(account_id, received_at_ms DESC, id DESC);

CREATE INDEX idx_messages_global_time
    ON messages(received_at_ms DESC, id DESC);

CREATE INDEX idx_messages_starred
    ON messages(received_at_ms DESC, id DESC)
    WHERE starred = 1;

CREATE INDEX idx_messages_unread
    ON messages(received_at_ms DESC, id DESC)
    WHERE unread = 1;

CREATE INDEX idx_outbox_pending
    ON outbox(state, next_attempt_at_ms, message_id);
