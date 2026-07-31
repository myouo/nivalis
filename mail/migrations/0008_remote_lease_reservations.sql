ALTER TABLE remote_change_intents
ADD COLUMN leased_folder_reserve INTEGER NOT NULL DEFAULT 0
CHECK (leased_folder_reserve BETWEEN 0 AND 512);

ALTER TABLE remote_journal_usage
ADD COLUMN reserved_count INTEGER NOT NULL DEFAULT 0
CHECK (reserved_count BETWEEN 0 AND 65536);

UPDATE remote_change_intents
SET state = CASE WHEN attempt_count >= 1000 THEN 'blocked' ELSE 'reconcile' END,
    leased_version = NULL,
    lease_expires_at_ms = NULL,
    reconcile_requested = CASE
        WHEN attempt_count < 1000 AND delete_requested = 0 THEN 1
        ELSE reconcile_requested
    END,
    error_class = CASE WHEN attempt_count >= 1000 THEN 'permanent' ELSE 'conflict' END,
    error_code = CASE
        WHEN attempt_count >= 1000 THEN 'attempt_limit'
        ELSE 'upgrade_lease_recovery'
    END,
    error_detail = CASE
        WHEN attempt_count >= 1000 THEN
            'Remote synchronization stopped after 1,000 attempts; review the account before retrying.'
        ELSE
            'An unfinished lease from the previous schema requires reconciliation before another write.'
    END
WHERE state = 'in_flight';

DROP TRIGGER enforce_remote_journal_child_limit_folders;
DROP TRIGGER enforce_remote_journal_child_limit_sources;
DROP TRIGGER enforce_remote_journal_child_limit_tombstones;

CREATE TRIGGER enforce_remote_journal_child_limit_folders
BEFORE INSERT ON remote_change_intent_folders
WHEN NOT EXISTS (
         SELECT 1 FROM remote_change_intent_folders
         WHERE intent_id = NEW.intent_id
           AND side = NEW.side
           AND folder_key = NEW.folder_key
     )
 AND (
     SELECT child_count + reserved_count
     FROM remote_journal_usage WHERE singleton = 1
 ) >= 65536
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
 AND (
     SELECT child_count + reserved_count
     FROM remote_journal_usage WHERE singleton = 1
 ) >= 65536
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
 AND (
     SELECT child_count + reserved_count
     FROM remote_journal_usage WHERE singleton = 1
 ) >= 65536
BEGIN
    SELECT RAISE(ABORT, 'remote journal child limit exceeded');
END;

CREATE TRIGGER validate_remote_lease_reserve_insert
BEFORE INSERT ON remote_change_intents
WHEN NEW.leased_folder_reserve <> 0
BEGIN
    SELECT RAISE(ABORT, 'remote lease reservation must be acquired by update');
END;

CREATE TRIGGER validate_remote_lease_reserve_update
BEFORE UPDATE OF state, leased_folder_reserve ON remote_change_intents
WHEN NEW.leased_folder_reserve <> 0 AND NEW.state <> 'in_flight'
BEGIN
    SELECT RAISE(ABORT, 'remote lease reservation requires an active lease');
END;

CREATE TRIGGER enforce_remote_lease_reserve_limit
BEFORE UPDATE OF leased_folder_reserve ON remote_change_intents
WHEN NEW.leased_folder_reserve > OLD.leased_folder_reserve
 AND (
     SELECT child_count + reserved_count
            + NEW.leased_folder_reserve - OLD.leased_folder_reserve
     FROM remote_journal_usage WHERE singleton = 1
 ) > 65536
BEGIN
    SELECT RAISE(ABORT, 'remote lease reservation limit exceeded');
END;

CREATE TRIGGER count_remote_lease_reserve_update
AFTER UPDATE OF leased_folder_reserve ON remote_change_intents
WHEN NEW.leased_folder_reserve <> OLD.leased_folder_reserve
BEGIN
    UPDATE remote_journal_usage
    SET reserved_count = reserved_count
        + NEW.leased_folder_reserve - OLD.leased_folder_reserve
    WHERE singleton = 1;
END;

CREATE TRIGGER count_remote_lease_reserve_delete
AFTER DELETE ON remote_change_intents
WHEN OLD.leased_folder_reserve <> 0
BEGIN
    UPDATE remote_journal_usage
    SET reserved_count = reserved_count - OLD.leased_folder_reserve
    WHERE singleton = 1;
END;
