CREATE INDEX idx_message_content_body_file
    ON message_content(body_file_key)
    WHERE body_file_key IS NOT NULL;

CREATE INDEX idx_attachments_file
    ON attachments(file_key, message_id)
    WHERE file_key IS NOT NULL;

CREATE INDEX idx_outbox_mime_file
    ON outbox(mime_file_key);
