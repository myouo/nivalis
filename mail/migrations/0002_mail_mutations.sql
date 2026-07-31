CREATE INDEX idx_folders_system_role
    ON folders(account_id, role)
    WHERE role IN ('inbox', 'archive', 'trash', 'sent', 'drafts');

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
       AND (id = NEW.id OR remote_key = NEW.remote_key)
 )
BEGIN
    SELECT RAISE(ABORT, 'duplicate system folder role');
END;

CREATE TRIGGER reject_duplicate_system_role_update
BEFORE UPDATE OF account_id, role ON folders
WHEN (NEW.account_id <> OLD.account_id OR NEW.role <> OLD.role)
 AND NEW.role IN ('inbox', 'archive', 'trash', 'sent', 'drafts')
 AND EXISTS (
     SELECT 1 FROM folders
     WHERE account_id = NEW.account_id AND role = NEW.role AND id <> OLD.id
 )
BEGIN
    SELECT RAISE(ABORT, 'duplicate system folder role');
END;

CREATE TABLE trash_undo (
    slot           INTEGER PRIMARY KEY
                   CHECK (slot = 1),
    token          INTEGER NOT NULL DEFAULT 0
                   CHECK (token BETWEEN 0 AND 9223372036854775807),
    message_id     INTEGER
                   CHECK (message_id IS NULL OR message_id > 0),
    account_id     INTEGER
                   CHECK (account_id IS NULL OR account_id > 0),
    expires_at_ms  INTEGER
                   CHECK (
                       expires_at_ms IS NULL OR
                       expires_at_ms BETWEEN -62135596800000 AND 253402300799999
                   ),
    folder_count   INTEGER NOT NULL DEFAULT 0
                   CHECK (folder_count BETWEEN 0 AND 256),
    CHECK (
        (
            message_id IS NULL AND
            account_id IS NULL AND
            expires_at_ms IS NULL AND
            folder_count = 0
        ) OR
        (
            token > 0 AND
            message_id IS NOT NULL AND
            account_id IS NOT NULL AND
            expires_at_ms IS NOT NULL AND
            folder_count > 0
        )
    )
) STRICT;

INSERT INTO trash_undo (slot) VALUES (1);

CREATE TABLE trash_undo_folders (
    slot        INTEGER NOT NULL DEFAULT 1
                REFERENCES trash_undo(slot) ON DELETE CASCADE,
    folder_id   INTEGER NOT NULL CHECK (folder_id > 0),
    account_id  INTEGER NOT NULL CHECK (account_id > 0),
    PRIMARY KEY (slot, folder_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE message_tombstones (
    account_id     INTEGER NOT NULL
                   REFERENCES accounts(id) ON DELETE CASCADE,
    remote_key     TEXT NOT NULL
                   CHECK (length(CAST(remote_key AS BLOB)) BETWEEN 1 AND 512),
    deleted_at_ms  INTEGER NOT NULL
                   CHECK (
                       deleted_at_ms BETWEEN -62135596800000 AND 253402300799999
                   ),
    PRIMARY KEY (account_id, remote_key)
) STRICT, WITHOUT ROWID;

CREATE TABLE file_gc (
    file_key      TEXT PRIMARY KEY
                  CHECK (length(CAST(file_key AS BLOB)) BETWEEN 1 AND 512),
    queued_at_ms  INTEGER NOT NULL
                  CHECK (
                      queued_at_ms BETWEEN -62135596800000 AND 253402300799999
                  )
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_message_tombstones_deleted
    ON message_tombstones(deleted_at_ms);

CREATE INDEX idx_file_gc_queued
    ON file_gc(queued_at_ms);

CREATE TRIGGER clear_trash_undo_before_message_delete
BEFORE DELETE ON messages
WHEN (SELECT message_id FROM trash_undo WHERE slot = 1) = OLD.id
BEGIN
    DELETE FROM trash_undo_folders WHERE slot = 1;
    UPDATE trash_undo
    SET message_id = NULL,
        account_id = NULL,
        expires_at_ms = NULL,
        folder_count = 0
    WHERE slot = 1;
END;
