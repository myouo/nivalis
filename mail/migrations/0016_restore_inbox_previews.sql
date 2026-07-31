-- Metadata-only sync used to persist an empty preview and then advance the
-- forward cursor beyond those messages. Re-open only affected inbox snapshots
-- once so the newest visible page is refreshed immediately; bounded history
-- loading will repair older cached rows as the user scrolls.
UPDATE sync_state
SET change_cursor = NULL,
    history_cursor = NULL,
    history_complete = 0
WHERE uid_validity IS NOT NULL
  AND change_cursor IS NOT NULL
  AND EXISTS (
      SELECT 1
      FROM imap_message_locations AS location
      JOIN messages AS message ON message.id = location.message_id
      WHERE location.folder_id = sync_state.folder_id
        AND location.uid_validity = sync_state.uid_validity
        AND message.preview = ''
        AND NOT EXISTS (
            SELECT 1
            FROM message_content AS content
            WHERE content.message_id = message.id
        )
  );
