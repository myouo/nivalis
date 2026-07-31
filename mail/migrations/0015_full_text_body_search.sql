DROP TRIGGER sync_message_search_insert;
DROP TRIGGER sync_message_search_delete;
DROP TRIGGER sync_message_search_update;
DROP TABLE message_search;

CREATE TABLE message_search_documents (
    rowid           INTEGER PRIMARY KEY
                    REFERENCES messages(id) ON DELETE CASCADE,
    sender_name     TEXT NOT NULL
                    CHECK (length(CAST(sender_name AS BLOB)) <= 320),
    sender_address  TEXT NOT NULL
                    CHECK (length(CAST(sender_address AS BLOB)) <= 320),
    subject         TEXT NOT NULL
                    CHECK (length(CAST(subject AS BLOB)) <= 998),
    preview         TEXT NOT NULL DEFAULT ''
                    CHECK (length(CAST(preview AS BLOB)) <= 2048),
    body            TEXT NOT NULL DEFAULT ''
                    CHECK (length(CAST(body AS BLOB)) <= 65536)
) STRICT;

CREATE TABLE message_search_backfill (
    singleton        INTEGER PRIMARY KEY CHECK (singleton = 1),
    next_message_id  INTEGER NOT NULL DEFAULT 1 CHECK (next_message_id > 0),
    complete         INTEGER NOT NULL DEFAULT 0 CHECK (complete IN (0, 1))
) STRICT;

INSERT INTO message_search_backfill (singleton) VALUES (1);

CREATE VIRTUAL TABLE message_search USING fts5(
    sender_name,
    sender_address,
    subject,
    preview,
    body,
    content = 'message_search_documents',
    content_rowid = 'rowid',
    tokenize = 'trigram case_sensitive 0',
    columnsize = 0
);

CREATE TRIGGER sync_message_search_document_insert
AFTER INSERT ON message_search_documents
BEGIN
    INSERT INTO message_search (
        rowid, sender_name, sender_address, subject, preview, body
    ) VALUES (
        NEW.rowid, NEW.sender_name, NEW.sender_address, NEW.subject, NEW.preview, NEW.body
    );
END;

CREATE TRIGGER sync_message_search_document_delete
AFTER DELETE ON message_search_documents
BEGIN
    INSERT INTO message_search (
        message_search, rowid, sender_name, sender_address, subject, preview, body
    ) VALUES (
        'delete', OLD.rowid, OLD.sender_name, OLD.sender_address, OLD.subject, OLD.preview, OLD.body
    );
END;

CREATE TRIGGER sync_message_search_document_update
AFTER UPDATE OF rowid, sender_name, sender_address, subject, preview, body
ON message_search_documents
BEGIN
    INSERT INTO message_search (
        message_search, rowid, sender_name, sender_address, subject, preview, body
    ) VALUES (
        'delete', OLD.rowid, OLD.sender_name, OLD.sender_address, OLD.subject, OLD.preview, OLD.body
    );
    INSERT INTO message_search (
        rowid, sender_name, sender_address, subject, preview, body
    ) VALUES (
        NEW.rowid, NEW.sender_name, NEW.sender_address, NEW.subject, NEW.preview, NEW.body
    );
END;

CREATE TRIGGER sync_message_search_insert
AFTER INSERT ON messages
BEGIN
    INSERT INTO message_search_documents (
        rowid, sender_name, sender_address, subject, preview, body
    ) VALUES (
        NEW.id, NEW.sender_name, NEW.sender_address, NEW.subject, NEW.preview, ''
    );
END;

CREATE TRIGGER sync_message_search_update
AFTER UPDATE OF id, sender_name, sender_address, subject, preview ON messages
BEGIN
    UPDATE message_search_documents
       SET rowid = NEW.id,
           sender_name = NEW.sender_name,
           sender_address = NEW.sender_address,
           subject = NEW.subject,
           preview = NEW.preview
     WHERE rowid = OLD.id;
END;

CREATE TRIGGER sync_message_search_body_insert
AFTER INSERT ON message_content
BEGIN
    UPDATE message_search_documents
       SET body = NEW.reader_excerpt
     WHERE rowid = NEW.message_id;
END;

CREATE TRIGGER sync_message_search_body_update
AFTER UPDATE OF message_id, reader_excerpt ON message_content
BEGIN
    UPDATE message_search_documents SET body = '' WHERE rowid = OLD.message_id;
    UPDATE message_search_documents
       SET body = NEW.reader_excerpt
     WHERE rowid = NEW.message_id;
END;

CREATE TRIGGER sync_message_search_body_delete
AFTER DELETE ON message_content
BEGIN
    UPDATE message_search_documents SET body = '' WHERE rowid = OLD.message_id;
END;
