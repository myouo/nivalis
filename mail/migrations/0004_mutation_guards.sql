DROP TRIGGER reject_duplicate_system_role_insert;

CREATE TRIGGER reject_duplicate_system_role_insert
BEFORE INSERT ON folders
WHEN NEW.role IN ('inbox', 'archive', 'trash', 'sent', 'drafts')
 AND EXISTS (
     SELECT 1 FROM folders
     WHERE account_id = NEW.account_id AND role = NEW.role
 )
 AND NOT EXISTS (
     SELECT 1 FROM folders
     WHERE account_id = NEW.account_id
       AND role = NEW.role
       AND (id = NEW.id OR remote_key = NEW.remote_key)
 )
BEGIN
    SELECT RAISE(ABORT, 'duplicate system folder role');
END;

CREATE TRIGGER dequeue_body_file_on_insert
AFTER INSERT ON message_content
WHEN NEW.body_file_key IS NOT NULL
BEGIN
    DELETE FROM file_gc WHERE file_key = NEW.body_file_key;
END;

CREATE TRIGGER dequeue_body_file_on_update
AFTER UPDATE OF body_file_key ON message_content
WHEN NEW.body_file_key IS NOT NULL
BEGIN
    DELETE FROM file_gc WHERE file_key = NEW.body_file_key;
END;

CREATE TRIGGER dequeue_attachment_file_on_insert
AFTER INSERT ON attachments
WHEN NEW.file_key IS NOT NULL
BEGIN
    DELETE FROM file_gc WHERE file_key = NEW.file_key;
END;

CREATE TRIGGER dequeue_attachment_file_on_update
AFTER UPDATE OF file_key ON attachments
WHEN NEW.file_key IS NOT NULL
BEGIN
    DELETE FROM file_gc WHERE file_key = NEW.file_key;
END;

CREATE TRIGGER dequeue_outbox_file_on_insert
AFTER INSERT ON outbox
BEGIN
    DELETE FROM file_gc WHERE file_key = NEW.mime_file_key;
END;

CREATE TRIGGER dequeue_outbox_file_on_update
AFTER UPDATE OF mime_file_key ON outbox
BEGIN
    DELETE FROM file_gc WHERE file_key = NEW.mime_file_key;
END;
