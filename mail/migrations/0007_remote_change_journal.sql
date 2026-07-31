ALTER TABLE sync_state ADD COLUMN highest_modseq INTEGER
    CHECK (highest_modseq IS NULL OR highest_modseq BETWEEN 1 AND 9223372036854775807);

ALTER TABLE sync_state ADD COLUMN mailbox_object_id TEXT
    CHECK (
        mailbox_object_id IS NULL OR
        length(CAST(mailbox_object_id AS BLOB)) BETWEEN 1 AND 512
    );

ALTER TABLE messages ADD COLUMN legacy_reconcile_revision INTEGER
    CHECK (
        legacy_reconcile_revision IS NULL OR (
            legacy_reconcile_revision BETWEEN 1 AND 9223372036854775807 AND
            legacy_reconcile_revision <= revision
        )
    );

CREATE TABLE account_object_states (
    account_id     INTEGER NOT NULL
                   REFERENCES accounts(id) ON DELETE CASCADE,
    object_kind    TEXT NOT NULL
                   CHECK (object_kind IN ('email', 'mailbox', 'thread')),
    state_token    TEXT NOT NULL
                   CHECK (length(CAST(state_token AS BLOB)) BETWEEN 1 AND 512),
    updated_at_ms  INTEGER NOT NULL
                   CHECK (updated_at_ms BETWEEN -62135596800000 AND 253402300799999),
    PRIMARY KEY (account_id, object_kind)
) STRICT, WITHOUT ROWID;

CREATE TABLE remote_account_reconciliations (
    account_id       INTEGER PRIMARY KEY
                     REFERENCES accounts(id) ON DELETE CASCADE,
    reason           TEXT NOT NULL
                     CHECK (reason = 'legacy_journal_bootstrap'),
    requested_at_ms  INTEGER NOT NULL
                     CHECK (
                         requested_at_ms BETWEEN -62135596800000 AND 253402300799999
                     )
) STRICT, WITHOUT ROWID;

CREATE TABLE imap_message_locations (
    message_id      INTEGER NOT NULL,
    folder_id       INTEGER NOT NULL,
    account_id      INTEGER NOT NULL,
    uid_validity    INTEGER NOT NULL
                    CHECK (uid_validity BETWEEN 1 AND 4294967295),
    uid             INTEGER NOT NULL
                    CHECK (uid BETWEEN 1 AND 4294967295),
    modseq          INTEGER
                    CHECK (modseq IS NULL OR modseq BETWEEN 1 AND 9223372036854775807),
    email_id        TEXT
                    CHECK (
                        email_id IS NULL OR
                        length(CAST(email_id AS BLOB)) BETWEEN 1 AND 512
                    ),
    remote_seen     INTEGER NOT NULL
                    CHECK (remote_seen IN (0, 1)),
    remote_flagged  INTEGER NOT NULL
                    CHECK (remote_flagged IN (0, 1)),
    PRIMARY KEY (message_id, folder_id),
    UNIQUE (folder_id, uid_validity, uid),
    FOREIGN KEY (message_id, folder_id)
        REFERENCES message_folders(message_id, folder_id) ON DELETE CASCADE,
    FOREIGN KEY (message_id, account_id)
        REFERENCES messages(id, account_id) ON DELETE CASCADE,
    FOREIGN KEY (folder_id, account_id)
        REFERENCES folders(id, account_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE remote_change_intents (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT
                          CHECK (id > 0),
    account_id            INTEGER NOT NULL
                          REFERENCES accounts(id) ON DELETE CASCADE,
    message_id            INTEGER
                          REFERENCES messages(id) ON DELETE SET NULL,
    target_key            TEXT NOT NULL
                          CHECK (length(CAST(target_key AS BLOB)) BETWEEN 1 AND 512),
    intent_version        INTEGER NOT NULL DEFAULT 1
                          CHECK (intent_version BETWEEN 1 AND 9223372036854775807),
    local_revision        INTEGER NOT NULL
                          CHECK (local_revision BETWEEN 0 AND 9223372036854775807),
    unread_base           INTEGER CHECK (unread_base IN (0, 1)),
    unread_desired        INTEGER CHECK (unread_desired IN (0, 1)),
    starred_base          INTEGER CHECK (starred_base IN (0, 1)),
    starred_desired       INTEGER CHECK (starred_desired IN (0, 1)),
    placement_active      INTEGER NOT NULL DEFAULT 0
                          CHECK (placement_active IN (0, 1)),
    reconcile_requested   INTEGER NOT NULL DEFAULT 0
                          CHECK (reconcile_requested IN (0, 1)),
    delete_requested      INTEGER NOT NULL DEFAULT 0
                          CHECK (delete_requested IN (0, 1)),
    dispatched_mask       INTEGER NOT NULL DEFAULT 0
                          CHECK (dispatched_mask BETWEEN 0 AND 15),
    state                 TEXT NOT NULL DEFAULT 'ready'
                          CHECK (state IN (
                              'ready', 'retry_wait', 'in_flight', 'reconcile', 'blocked'
                          )),
    leased_version        INTEGER
                          CHECK (
                              leased_version IS NULL OR
                              leased_version BETWEEN 1 AND 9223372036854775807
                          ),
    claim_epoch           INTEGER NOT NULL DEFAULT 0
                          CHECK (claim_epoch BETWEEN 0 AND 9223372036854775807),
    lease_expires_at_ms   INTEGER
                          CHECK (
                              lease_expires_at_ms IS NULL OR
                              lease_expires_at_ms BETWEEN -62135596800000 AND 253402300799999
                          ),
    attempt_count         INTEGER NOT NULL DEFAULT 0
                          CHECK (attempt_count BETWEEN 0 AND 1000),
    not_before_ms         INTEGER NOT NULL
                          CHECK (not_before_ms BETWEEN -62135596800000 AND 253402300799999),
    error_class           TEXT
                          CHECK (
                              error_class IS NULL OR error_class IN (
                                  'network', 'rate_limit', 'auth', 'conflict', 'permanent'
                              )
                          ),
    error_code            TEXT
                          CHECK (
                              error_code IS NULL OR
                              length(CAST(error_code AS BLOB)) BETWEEN 1 AND 64
                          ),
    error_detail          TEXT
                          CHECK (
                              error_detail IS NULL OR
                              length(CAST(error_detail AS BLOB)) BETWEEN 1 AND 1024
                          ),
    created_at_ms         INTEGER NOT NULL
                          CHECK (created_at_ms BETWEEN -62135596800000 AND 253402300799999),
    updated_at_ms         INTEGER NOT NULL
                          CHECK (updated_at_ms BETWEEN -62135596800000 AND 253402300799999),
    UNIQUE (account_id, target_key),
    CHECK ((unread_base IS NULL) = (unread_desired IS NULL)),
    CHECK ((starred_base IS NULL) = (starred_desired IS NULL)),
    CHECK (
        delete_requested = 0 OR (
            unread_base IS NULL AND
            starred_base IS NULL AND
            placement_active = 0 AND
            reconcile_requested = 0
        )
    ),
    CHECK (
        delete_requested = 1 OR
        unread_base IS NOT NULL OR
        starred_base IS NOT NULL OR
        placement_active = 1 OR
        reconcile_requested = 1
    ),
    CHECK (leased_version IS NULL OR leased_version <= intent_version),
    CHECK (
        (state = 'in_flight') =
        (leased_version IS NOT NULL AND lease_expires_at_ms IS NOT NULL)
    ),
    CHECK ((leased_version IS NULL) = (lease_expires_at_ms IS NULL)),
    CHECK (state <> 'in_flight' OR (claim_epoch > 0 AND attempt_count > 0))
) STRICT;

CREATE TABLE remote_change_intent_folders (
    intent_id   INTEGER NOT NULL
                REFERENCES remote_change_intents(id) ON DELETE CASCADE,
    side        TEXT NOT NULL CHECK (side IN ('base', 'desired')),
    folder_key  TEXT NOT NULL
                CHECK (length(CAST(folder_key AS BLOB)) BETWEEN 1 AND 512),
    PRIMARY KEY (intent_id, side, folder_key)
) STRICT, WITHOUT ROWID;

CREATE TABLE remote_change_intent_imap_sources (
    intent_id          INTEGER NOT NULL
                       REFERENCES remote_change_intents(id) ON DELETE CASCADE,
    folder_key         TEXT NOT NULL
                       CHECK (length(CAST(folder_key AS BLOB)) BETWEEN 1 AND 512),
    mailbox_object_id  TEXT
                       CHECK (
                           mailbox_object_id IS NULL OR
                           length(CAST(mailbox_object_id AS BLOB)) BETWEEN 1 AND 512
                       ),
    uid_validity       INTEGER NOT NULL
                       CHECK (uid_validity BETWEEN 1 AND 4294967295),
    uid                INTEGER NOT NULL
                       CHECK (uid BETWEEN 1 AND 4294967295),
    modseq             INTEGER
                       CHECK (modseq IS NULL OR modseq BETWEEN 1 AND 9223372036854775807),
    email_id           TEXT
                       CHECK (
                           email_id IS NULL OR
                           length(CAST(email_id AS BLOB)) BETWEEN 1 AND 512
                       ),
    remote_seen        INTEGER NOT NULL
                       CHECK (remote_seen IN (0, 1)),
    remote_flagged     INTEGER NOT NULL
                       CHECK (remote_flagged IN (0, 1)),
    PRIMARY KEY (intent_id, folder_key, uid_validity, uid)
) STRICT, WITHOUT ROWID;

CREATE TABLE message_tombstone_imap_locations (
    account_id         INTEGER NOT NULL,
    target_key         TEXT NOT NULL,
    folder_key         TEXT NOT NULL
                       CHECK (length(CAST(folder_key AS BLOB)) BETWEEN 1 AND 512),
    mailbox_object_id  TEXT
                       CHECK (
                           mailbox_object_id IS NULL OR
                           length(CAST(mailbox_object_id AS BLOB)) BETWEEN 1 AND 512
                       ),
    uid_validity       INTEGER NOT NULL
                       CHECK (uid_validity BETWEEN 1 AND 4294967295),
    uid                INTEGER NOT NULL
                       CHECK (uid BETWEEN 1 AND 4294967295),
    email_id           TEXT
                       CHECK (
                           email_id IS NULL OR
                           length(CAST(email_id AS BLOB)) BETWEEN 1 AND 512
                       ),
    PRIMARY KEY (account_id, target_key, folder_key, uid_validity, uid),
    FOREIGN KEY (account_id, target_key)
        REFERENCES message_tombstones(account_id, remote_key) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE remote_journal_usage (
    singleton    INTEGER PRIMARY KEY CHECK (singleton = 1),
    child_count  INTEGER NOT NULL DEFAULT 0
                 CHECK (child_count BETWEEN 0 AND 65536)
) STRICT;

INSERT INTO remote_journal_usage (singleton) VALUES (1);

CREATE INDEX idx_remote_intents_account_due
    ON remote_change_intents(account_id, state, not_before_ms, id);

CREATE INDEX idx_remote_intents_global_due
    ON remote_change_intents(state, not_before_ms, id);

CREATE INDEX idx_messages_legacy_reconcile_pending
    ON messages(account_id, id)
    WHERE legacy_reconcile_revision IS NOT NULL;

CREATE TRIGGER validate_remote_intent_message_insert
BEFORE INSERT ON remote_change_intents
WHEN NEW.message_id IS NOT NULL
 AND NOT EXISTS (
     SELECT 1 FROM messages
     WHERE id = NEW.message_id
       AND account_id = NEW.account_id
       AND remote_key = NEW.target_key
 )
BEGIN
    SELECT RAISE(ABORT, 'remote intent does not match its message identity');
END;

CREATE TRIGGER validate_remote_intent_message_update
BEFORE UPDATE OF message_id, account_id, target_key ON remote_change_intents
WHEN NEW.message_id IS NOT NULL
 AND NOT EXISTS (
     SELECT 1 FROM messages
     WHERE id = NEW.message_id
       AND account_id = NEW.account_id
       AND remote_key = NEW.target_key
 )
BEGIN
    SELECT RAISE(ABORT, 'remote intent does not match its message identity');
END;

CREATE TRIGGER reject_remote_intent_identity_update
BEFORE UPDATE OF account_id, target_key ON remote_change_intents
WHEN NEW.account_id <> OLD.account_id OR NEW.target_key <> OLD.target_key
BEGIN
    SELECT RAISE(ABORT, 'remote intent identity is immutable');
END;

CREATE TRIGGER reject_message_remote_identity_update
BEFORE UPDATE OF account_id, remote_key ON messages
WHEN NEW.account_id <> OLD.account_id OR NEW.remote_key <> OLD.remote_key
BEGIN
    SELECT RAISE(ABORT, 'message remote identity is immutable');
END;

CREATE TRIGGER reject_account_remote_identity_update
BEFORE UPDATE OF provider, remote_key ON accounts
WHEN NEW.provider <> OLD.provider OR NEW.remote_key <> OLD.remote_key
BEGIN
    SELECT RAISE(ABORT, 'account remote identity is immutable');
END;

CREATE TRIGGER reject_folder_account_update
BEFORE UPDATE OF account_id ON folders
WHEN NEW.account_id <> OLD.account_id
BEGIN
    SELECT RAISE(ABORT, 'folder account is immutable');
END;

CREATE TRIGGER reject_remote_intent_version_regression
BEFORE UPDATE OF intent_version, claim_epoch ON remote_change_intents
WHEN NEW.intent_version < OLD.intent_version OR NEW.claim_epoch < OLD.claim_epoch
BEGIN
    SELECT RAISE(ABORT, 'remote intent version cannot move backwards');
END;

CREATE TRIGGER enforce_remote_intent_limits
BEFORE INSERT ON remote_change_intents
WHEN NOT EXISTS (
         SELECT 1 FROM remote_change_intents
         WHERE account_id = NEW.account_id AND target_key = NEW.target_key
     )
 AND (
     EXISTS (
         SELECT 1 FROM remote_change_intents
         WHERE account_id = NEW.account_id
         LIMIT 1 OFFSET 4095
     )
     OR EXISTS (SELECT 1 FROM remote_change_intents LIMIT 1 OFFSET 16383)
 )
BEGIN
    SELECT RAISE(ABORT, 'remote intent limit exceeded');
END;

CREATE TRIGGER enforce_remote_intent_folder_limit
BEFORE INSERT ON remote_change_intent_folders
WHEN NOT EXISTS (
    SELECT 1 FROM remote_change_intent_folders
    WHERE intent_id = NEW.intent_id AND side = NEW.side AND folder_key = NEW.folder_key
)
 AND EXISTS (
    SELECT 1 FROM remote_change_intent_folders
    WHERE intent_id = NEW.intent_id AND side = NEW.side
    LIMIT 1 OFFSET 255
)
BEGIN
    SELECT RAISE(ABORT, 'remote intent folder limit exceeded');
END;

CREATE TRIGGER require_active_remote_intent_placement
BEFORE INSERT ON remote_change_intent_folders
WHEN NOT EXISTS (
        SELECT 1 FROM remote_change_intent_folders
        WHERE intent_id = NEW.intent_id
          AND side = NEW.side
          AND folder_key = NEW.folder_key
    )
 AND NOT EXISTS (
    SELECT 1 FROM remote_change_intents
    WHERE id = NEW.intent_id AND placement_active = 1
)
BEGIN
    SELECT RAISE(ABORT, 'remote intent placement is not active');
END;

CREATE TRIGGER protect_remote_intent_folder_snapshot
BEFORE UPDATE OF placement_active ON remote_change_intents
WHEN NEW.placement_active = 0
 AND EXISTS (
     SELECT 1 FROM remote_change_intent_folders WHERE intent_id = OLD.id
 )
BEGIN
    SELECT RAISE(ABORT, 'remote intent folder snapshot must be cleared first');
END;

CREATE TRIGGER reject_remote_intent_folder_identity_update
BEFORE UPDATE OF intent_id, side, folder_key ON remote_change_intent_folders
WHEN NEW.intent_id <> OLD.intent_id
  OR NEW.side <> OLD.side
  OR NEW.folder_key <> OLD.folder_key
BEGIN
    SELECT RAISE(ABORT, 'remote intent folder identity is immutable');
END;

CREATE TRIGGER enforce_remote_intent_source_limit
BEFORE INSERT ON remote_change_intent_imap_sources
WHEN NOT EXISTS (
    SELECT 1 FROM remote_change_intent_imap_sources
    WHERE intent_id = NEW.intent_id
      AND folder_key = NEW.folder_key
      AND uid_validity = NEW.uid_validity
      AND uid = NEW.uid
)
 AND EXISTS (
    SELECT 1 FROM remote_change_intent_imap_sources
    WHERE intent_id = NEW.intent_id
    LIMIT 1 OFFSET 255
)
BEGIN
    SELECT RAISE(ABORT, 'remote intent source limit exceeded');
END;

CREATE TRIGGER reject_remote_intent_source_identity_update
BEFORE UPDATE OF intent_id, folder_key, uid_validity, uid
ON remote_change_intent_imap_sources
WHEN NEW.intent_id <> OLD.intent_id
  OR NEW.folder_key <> OLD.folder_key
  OR NEW.uid_validity <> OLD.uid_validity
  OR NEW.uid <> OLD.uid
BEGIN
    SELECT RAISE(ABORT, 'remote intent source identity is immutable');
END;

CREATE TRIGGER enforce_tombstone_location_limit
BEFORE INSERT ON message_tombstone_imap_locations
WHEN NOT EXISTS (
    SELECT 1 FROM message_tombstone_imap_locations
    WHERE account_id = NEW.account_id
      AND target_key = NEW.target_key
      AND folder_key = NEW.folder_key
      AND uid_validity = NEW.uid_validity
      AND uid = NEW.uid
)
 AND EXISTS (
    SELECT 1 FROM message_tombstone_imap_locations
    WHERE account_id = NEW.account_id AND target_key = NEW.target_key
    LIMIT 1 OFFSET 255
)
BEGIN
    SELECT RAISE(ABORT, 'tombstone location limit exceeded');
END;

CREATE TRIGGER reject_tombstone_location_identity_update
BEFORE UPDATE OF account_id, target_key, folder_key, uid_validity, uid
ON message_tombstone_imap_locations
WHEN NEW.account_id <> OLD.account_id
  OR NEW.target_key <> OLD.target_key
  OR NEW.folder_key <> OLD.folder_key
  OR NEW.uid_validity <> OLD.uid_validity
  OR NEW.uid <> OLD.uid
BEGIN
    SELECT RAISE(ABORT, 'tombstone location identity is immutable');
END;

CREATE TRIGGER enforce_remote_journal_child_limit_folders
BEFORE INSERT ON remote_change_intent_folders
WHEN NOT EXISTS (
         SELECT 1 FROM remote_change_intent_folders
         WHERE intent_id = NEW.intent_id
           AND side = NEW.side
           AND folder_key = NEW.folder_key
     )
 AND (SELECT child_count FROM remote_journal_usage WHERE singleton = 1) >= 65536
BEGIN
    SELECT RAISE(ABORT, 'remote journal child limit exceeded');
END;

CREATE TRIGGER enforce_remote_journal_child_limit_sources
BEFORE INSERT ON remote_change_intent_imap_sources
WHEN NOT EXISTS (
         SELECT 1 FROM remote_change_intent_imap_sources
         WHERE intent_id = NEW.intent_id
           AND folder_key = NEW.folder_key
           AND uid_validity = NEW.uid_validity
           AND uid = NEW.uid
     )
 AND (SELECT child_count FROM remote_journal_usage WHERE singleton = 1) >= 65536
BEGIN
    SELECT RAISE(ABORT, 'remote journal child limit exceeded');
END;

CREATE TRIGGER enforce_remote_journal_child_limit_tombstones
BEFORE INSERT ON message_tombstone_imap_locations
WHEN NOT EXISTS (
         SELECT 1 FROM message_tombstone_imap_locations
         WHERE account_id = NEW.account_id
           AND target_key = NEW.target_key
           AND folder_key = NEW.folder_key
           AND uid_validity = NEW.uid_validity
           AND uid = NEW.uid
     )
 AND (SELECT child_count FROM remote_journal_usage WHERE singleton = 1) >= 65536
BEGIN
    SELECT RAISE(ABORT, 'remote journal child limit exceeded');
END;

CREATE TRIGGER count_remote_intent_folder_insert
AFTER INSERT ON remote_change_intent_folders
BEGIN
    UPDATE remote_journal_usage
    SET child_count = child_count + 1
    WHERE singleton = 1;
END;

CREATE TRIGGER count_remote_intent_folder_delete
AFTER DELETE ON remote_change_intent_folders
BEGIN
    UPDATE remote_journal_usage
    SET child_count = child_count - 1
    WHERE singleton = 1;
END;

CREATE TRIGGER count_remote_intent_source_insert
AFTER INSERT ON remote_change_intent_imap_sources
BEGIN
    UPDATE remote_journal_usage
    SET child_count = child_count + 1
    WHERE singleton = 1;
END;

CREATE TRIGGER count_remote_intent_source_delete
AFTER DELETE ON remote_change_intent_imap_sources
BEGIN
    UPDATE remote_journal_usage
    SET child_count = child_count - 1
    WHERE singleton = 1;
END;

CREATE TRIGGER count_tombstone_location_insert
AFTER INSERT ON message_tombstone_imap_locations
BEGIN
    UPDATE remote_journal_usage
    SET child_count = child_count + 1
    WHERE singleton = 1;
END;

CREATE TRIGGER count_tombstone_location_delete
AFTER DELETE ON message_tombstone_imap_locations
BEGIN
    UPDATE remote_journal_usage
    SET child_count = child_count - 1
    WHERE singleton = 1;
END;

UPDATE messages
SET legacy_reconcile_revision = revision
WHERE revision > 0;

INSERT INTO remote_account_reconciliations (
    account_id,
    reason,
    requested_at_ms
)
SELECT
    accounts.id,
    'legacy_journal_bootstrap',
    0
FROM accounts
WHERE EXISTS (
        SELECT 1
        FROM messages
        WHERE messages.account_id = accounts.id
          AND messages.legacy_reconcile_revision IS NOT NULL
    )
   OR EXISTS (
        SELECT 1
        FROM message_tombstones
        WHERE message_tombstones.account_id = accounts.id
    );
