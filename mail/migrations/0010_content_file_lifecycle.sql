ALTER TABLE messages
ADD COLUMN content_generation INTEGER NOT NULL DEFAULT 0
CHECK (content_generation BETWEEN 0 AND 9223372036854775807);

CREATE TABLE file_staging (
    file_key            TEXT PRIMARY KEY
                        CHECK (
                            (
                                file_kind = 'body' AND
                                part_ordinal IS NULL AND
                                length(CAST(file_key AS BLOB)) = 41 AND
                                substr(file_key, 1, 5) = 'body/' AND
                                substr(file_key, 38, 4) = '.txt' AND
                                substr(file_key, 6, 32) NOT GLOB '*[^0-9a-f]*'
                            ) OR (
                                file_kind = 'attachment' AND
                                part_ordinal IS NOT NULL AND
                                part_ordinal BETWEEN 0 AND 31 AND
                                length(CAST(file_key AS BLOB)) = 47 AND
                                substr(file_key, 1, 11) = 'attachment/' AND
                                substr(file_key, 44, 4) = '.bin' AND
                                substr(file_key, 12, 32) NOT GLOB '*[^0-9a-f]*'
                            )
                        ),
    batch_token         TEXT NOT NULL
                        CHECK (
                            length(CAST(batch_token AS BLOB)) = 32 AND
                            batch_token NOT GLOB '*[^0-9a-f]*'
                        ),
    message_id          INTEGER NOT NULL CHECK (message_id > 0),
    account_id          INTEGER NOT NULL CHECK (account_id > 0),
    content_generation  INTEGER NOT NULL
                        CHECK (content_generation BETWEEN 1 AND 9223372036854775807),
    file_kind           TEXT NOT NULL CHECK (file_kind IN ('body', 'attachment')),
    part_ordinal        INTEGER,
    created_at_ms       INTEGER NOT NULL
                        CHECK (
                            created_at_ms BETWEEN -62135596800000 AND 253402300799999
                        ),
    expires_at_ms       INTEGER NOT NULL
                        CHECK (
                            expires_at_ms BETWEEN -62135596800000 AND 253402300799999 AND
                            expires_at_ms > created_at_ms
                        )
) STRICT, WITHOUT ROWID;

CREATE TABLE file_staging_usage (
    singleton   INTEGER PRIMARY KEY CHECK (singleton = 1),
    file_count  INTEGER NOT NULL DEFAULT 0
                CHECK (file_count BETWEEN 0 AND 256)
) STRICT;

INSERT INTO file_staging_usage (singleton) VALUES (1);

CREATE INDEX idx_file_staging_batch
    ON file_staging(batch_token, file_key);

CREATE UNIQUE INDEX idx_file_staging_batch_body
    ON file_staging(batch_token)
    WHERE file_kind = 'body';

CREATE UNIQUE INDEX idx_file_staging_batch_attachment
    ON file_staging(batch_token, part_ordinal)
    WHERE file_kind = 'attachment';

CREATE INDEX idx_file_staging_expiry
    ON file_staging(expires_at_ms, batch_token, file_key);

CREATE INDEX idx_file_staging_message
    ON file_staging(message_id, content_generation, batch_token, file_key);

CREATE TRIGGER enforce_file_staging_global_limit
BEFORE INSERT ON file_staging
WHEN (SELECT file_count FROM file_staging_usage WHERE singleton = 1) >= 256
BEGIN
    SELECT RAISE(ABORT, 'file staging global limit exceeded');
END;

CREATE TRIGGER enforce_file_staging_batch_limit
BEFORE INSERT ON file_staging
WHEN EXISTS (
    SELECT 1 FROM file_staging
    WHERE batch_token = NEW.batch_token
    LIMIT 1 OFFSET 32
)
BEGIN
    SELECT RAISE(ABORT, 'file staging batch limit exceeded');
END;

CREATE TRIGGER enforce_file_staging_batch_identity
BEFORE INSERT ON file_staging
WHEN EXISTS (
    SELECT 1 FROM file_staging
    WHERE batch_token = NEW.batch_token
      AND (
          message_id <> NEW.message_id OR
          account_id <> NEW.account_id OR
          content_generation <> NEW.content_generation OR
          created_at_ms <> NEW.created_at_ms OR
          expires_at_ms <> NEW.expires_at_ms
      )
)
BEGIN
    SELECT RAISE(ABORT, 'file staging batch identity mismatch');
END;

CREATE TRIGGER enforce_file_staging_message_generation_batch
BEFORE INSERT ON file_staging
WHEN EXISTS (
    SELECT 1 FROM file_staging
    WHERE message_id = NEW.message_id
      AND content_generation = NEW.content_generation
      AND batch_token <> NEW.batch_token
)
BEGIN
    SELECT RAISE(ABORT, 'message content generation already has a staging batch');
END;

CREATE TRIGGER reject_file_staging_update
BEFORE UPDATE ON file_staging
BEGIN
    SELECT RAISE(ABORT, 'file staging rows are immutable');
END;

CREATE TRIGGER reject_file_staging_usage_delete
BEFORE DELETE ON file_staging_usage
BEGIN
    SELECT RAISE(ABORT, 'file staging usage singleton cannot be deleted');
END;

CREATE TRIGGER count_file_staging_insert
AFTER INSERT ON file_staging
BEGIN
    UPDATE file_staging_usage
    SET file_count = file_count + 1
    WHERE singleton = 1;
END;

CREATE TRIGGER count_file_staging_delete
AFTER DELETE ON file_staging
BEGIN
    UPDATE file_staging_usage
    SET file_count = file_count - 1
    WHERE singleton = 1;
END;

CREATE TRIGGER dequeue_staged_file_on_insert
AFTER INSERT ON file_staging
BEGIN
    DELETE FROM file_gc WHERE file_key = NEW.file_key;
END;
