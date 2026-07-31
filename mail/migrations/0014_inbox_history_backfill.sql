ALTER TABLE sync_state ADD COLUMN history_cursor INTEGER
CHECK (history_cursor IS NULL OR history_cursor BETWEEN 1 AND 4294967295);

ALTER TABLE sync_state ADD COLUMN history_complete INTEGER NOT NULL DEFAULT 0
CHECK (history_complete IN (0, 1));

-- Existing clients advanced change_cursor to the newest cached UID even when older
-- mail was never imported. Start a bounded backward scan immediately below the
-- oldest cached UID so upgrading recovers history without re-downloading the tail.
UPDATE sync_state
SET history_cursor = CASE
        WHEN (
            SELECT MIN(location.uid)
            FROM imap_message_locations AS location
            WHERE location.folder_id = sync_state.folder_id
              AND location.uid_validity = sync_state.uid_validity
        ) > 1
        THEN (
            SELECT MIN(location.uid) - 1
            FROM imap_message_locations AS location
            WHERE location.folder_id = sync_state.folder_id
              AND location.uid_validity = sync_state.uid_validity
        )
        WHEN NOT EXISTS (
            SELECT 1
            FROM imap_message_locations AS location
            WHERE location.folder_id = sync_state.folder_id
              AND location.uid_validity = sync_state.uid_validity
        )
          AND change_cursor IS NOT NULL
          AND CAST(CAST(change_cursor AS INTEGER) AS TEXT) = change_cursor
          AND CAST(change_cursor AS INTEGER) BETWEEN 1 AND 4294967295
        THEN CAST(change_cursor AS INTEGER)
        ELSE NULL
    END,
    history_complete = CASE
        WHEN EXISTS (
            SELECT 1
            FROM imap_message_locations AS location
            WHERE location.folder_id = sync_state.folder_id
              AND location.uid_validity = sync_state.uid_validity
              AND location.uid = 1
        )
        THEN 1
        ELSE 0
    END;
