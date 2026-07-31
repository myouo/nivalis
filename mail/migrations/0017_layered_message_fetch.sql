-- Keep mailbox metadata independent from preview/body availability.  IMAP
-- messages with an empty preview need background discovery; locally-authored
-- messages and already-parsed content have a definitive preview state.
ALTER TABLE messages ADD COLUMN preview_state TEXT NOT NULL DEFAULT 'available'
    CHECK (preview_state IN ('missing', 'available', 'empty'));

ALTER TABLE messages ADD COLUMN preview_version INTEGER NOT NULL DEFAULT 1
    CHECK (preview_version BETWEEN 0 AND 2147483647);

ALTER TABLE messages ADD COLUMN remote_byte_count INTEGER NOT NULL DEFAULT 0
    CHECK (remote_byte_count BETWEEN 0 AND 1099511627776);

UPDATE messages
SET preview_state = CASE
        WHEN preview <> '' THEN 'available'
        WHEN EXISTS (
            SELECT 1 FROM message_content AS content
            WHERE content.message_id = messages.id
        ) THEN 'empty'
        WHEN EXISTS (
            SELECT 1 FROM imap_message_locations AS location
            WHERE location.message_id = messages.id
        ) THEN 'missing'
        ELSE 'empty'
    END,
    preview_version = CASE
        WHEN preview <> '' OR EXISTS (
            SELECT 1 FROM message_content AS content
            WHERE content.message_id = messages.id
        ) THEN 1
        ELSE 0
    END;

CREATE INDEX idx_messages_missing_preview
    ON messages(account_id, received_at_ms DESC, id DESC)
    WHERE preview_state = 'missing';
