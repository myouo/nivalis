DELETE FROM file_gc AS gc
WHERE EXISTS (
          SELECT 1 FROM message_content
          WHERE body_file_key = gc.file_key
      )
   OR EXISTS (
          SELECT 1 FROM attachments
          WHERE file_key = gc.file_key
      )
   OR EXISTS (
          SELECT 1 FROM outbox
          WHERE mime_file_key = gc.file_key
      );
