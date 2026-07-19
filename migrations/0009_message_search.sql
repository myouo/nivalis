CREATE VIRTUAL TABLE message_search USING fts5(
    sender_name,
    sender_address,
    subject,
    preview,
    content = 'messages',
    content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2',
    columnsize = 0
);

CREATE TRIGGER sync_message_search_insert
AFTER INSERT ON messages
BEGIN
    INSERT INTO message_search (
        rowid, sender_name, sender_address, subject, preview
    ) VALUES (
        NEW.id, NEW.sender_name, NEW.sender_address, NEW.subject, NEW.preview
    );
END;

CREATE TRIGGER sync_message_search_delete
AFTER DELETE ON messages
BEGIN
    INSERT INTO message_search (
        message_search, rowid, sender_name, sender_address, subject, preview
    ) VALUES (
        'delete', OLD.id, OLD.sender_name, OLD.sender_address, OLD.subject, OLD.preview
    );
END;

CREATE TRIGGER sync_message_search_update
AFTER UPDATE OF id, sender_name, sender_address, subject, preview ON messages
BEGIN
    INSERT INTO message_search (
        message_search, rowid, sender_name, sender_address, subject, preview
    ) VALUES (
        'delete', OLD.id, OLD.sender_name, OLD.sender_address, OLD.subject, OLD.preview
    );
    INSERT INTO message_search (
        rowid, sender_name, sender_address, subject, preview
    ) VALUES (
        NEW.id, NEW.sender_name, NEW.sender_address, NEW.subject, NEW.preview
    );
END;

INSERT INTO message_search(message_search) VALUES ('rebuild');
