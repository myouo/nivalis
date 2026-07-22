-- Migration 0016 cleared the forward cursor for any inbox containing an
-- uncached empty preview.  Existing IMAP locations were intentionally kept,
-- so the next metadata page could be rejected by the 50-row pending-window
-- guard as though every cached UID were newly pending.  Preview hydration is
-- independent from metadata sync as of schema 17; recover the cursor from the
-- durable locators instead of replaying the mailbox.
UPDATE sync_state
SET change_cursor = (
        SELECT CAST(MAX(location.uid) AS TEXT)
        FROM imap_message_locations AS location
        WHERE location.folder_id = sync_state.folder_id
          AND location.uid_validity = sync_state.uid_validity
    ),
    history_cursor = CASE
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
        ELSE NULL
    END,
    history_complete = CASE
        WHEN (
            SELECT MIN(location.uid)
            FROM imap_message_locations AS location
            WHERE location.folder_id = sync_state.folder_id
              AND location.uid_validity = sync_state.uid_validity
        ) = 1
        THEN 1
        ELSE 0
    END
WHERE change_cursor IS NULL
  AND uid_validity IS NOT NULL
  AND EXISTS (
      SELECT 1
      FROM imap_message_locations AS location
      WHERE location.folder_id = sync_state.folder_id
        AND location.uid_validity = sync_state.uid_validity
  );
