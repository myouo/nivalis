ALTER TABLE account_connections
ADD COLUMN smtp_host TEXT NOT NULL DEFAULT 'localhost'
CHECK (length(CAST(smtp_host AS BLOB)) BETWEEN 1 AND 253);

ALTER TABLE account_connections
ADD COLUMN smtp_port INTEGER NOT NULL DEFAULT 465
CHECK (smtp_port BETWEEN 1 AND 65535);

ALTER TABLE account_connections
ADD COLUMN smtp_security TEXT NOT NULL DEFAULT 'implicit_tls'
CHECK (smtp_security IN ('implicit_tls', 'starttls'));

ALTER TABLE account_connections
ADD COLUMN smtp_state TEXT NOT NULL DEFAULT 'needs_configuration'
CHECK (smtp_state IN ('needs_configuration', 'configured'));

UPDATE account_connections SET smtp_host = imap_host;

CREATE TABLE local_drafts (
    message_id     INTEGER PRIMARY KEY
                   REFERENCES messages(id) ON DELETE CASCADE,
    updated_at_ms  INTEGER NOT NULL
                   CHECK (
                       updated_at_ms BETWEEN -62135596800000 AND 253402300799999
                   ),
    locked_artifact_generation INTEGER
                   CHECK (
                       locked_artifact_generation IS NULL OR
                       locked_artifact_generation BETWEEN 1 AND 9223372036854775807
                   )
) STRICT;

CREATE TABLE draft_recipients (
    message_id    INTEGER NOT NULL
                  REFERENCES local_drafts(message_id) ON DELETE CASCADE,
    ordinal       INTEGER NOT NULL CHECK (ordinal BETWEEN 0 AND 63),
    address       TEXT NOT NULL
                  CHECK (length(CAST(address AS BLOB)) BETWEEN 1 AND 320),
    display_name  TEXT NOT NULL DEFAULT ''
                  CHECK (length(CAST(display_name AS BLOB)) <= 320),
    PRIMARY KEY (message_id, ordinal)
) STRICT, WITHOUT ROWID;

CREATE INDEX idx_local_drafts_updated
    ON local_drafts(updated_at_ms DESC, message_id DESC);

CREATE TRIGGER enforce_local_draft_limit
BEFORE INSERT ON local_drafts
WHEN NOT EXISTS (
         SELECT 1 FROM local_drafts WHERE message_id = NEW.message_id
     )
 AND (SELECT count(*) FROM local_drafts) >= 128
BEGIN
    SELECT RAISE(ABORT, 'local draft limit exceeded');
END;

CREATE TRIGGER enforce_local_draft_body_insert
BEFORE INSERT ON local_drafts
WHEN EXISTS (
    SELECT 1 FROM message_content
    WHERE message_id = NEW.message_id AND body_byte_count > 1048576
)
BEGIN
    SELECT RAISE(ABORT, 'local draft body limit exceeded');
END;

CREATE TRIGGER enforce_local_draft_body_content_insert
BEFORE INSERT ON message_content
WHEN NEW.body_byte_count > 1048576
 AND EXISTS (
     SELECT 1 FROM local_drafts WHERE message_id = NEW.message_id
 )
BEGIN
    SELECT RAISE(ABORT, 'local draft body limit exceeded');
END;

CREATE TRIGGER enforce_local_draft_body_content_update
BEFORE UPDATE OF body_byte_count ON message_content
WHEN NEW.body_byte_count > 1048576
 AND EXISTS (
     SELECT 1 FROM local_drafts WHERE message_id = NEW.message_id
 )
BEGIN
    SELECT RAISE(ABORT, 'local draft body limit exceeded');
END;

DROP INDEX idx_outbox_pending;
DROP INDEX idx_outbox_mime_file;
DROP TRIGGER dequeue_outbox_file_on_insert;
DROP TRIGGER dequeue_outbox_file_on_update;

ALTER TABLE outbox RENAME TO outbox_v11;
ALTER TABLE outbox_recipients RENAME TO outbox_recipients_v11;

CREATE TABLE outbox (
    message_id                INTEGER PRIMARY KEY
                              REFERENCES messages(id) ON DELETE CASCADE,
    account_id                INTEGER NOT NULL,
    configuration_generation  INTEGER NOT NULL
                              CHECK (
                                  configuration_generation BETWEEN 1 AND 9223372036854775807
                              ),
    artifact_generation       INTEGER NOT NULL
                              CHECK (artifact_generation BETWEEN 1 AND 9223372036854775807),
    draft_revision            INTEGER NOT NULL
                              CHECK (draft_revision BETWEEN 0 AND 9223372036854775807),
    reservation_token         TEXT
                              CHECK (
                                  reservation_token IS NULL OR (
                                      length(CAST(reservation_token AS BLOB)) = 32 AND
                                      reservation_token NOT GLOB '*[^0-9a-f]*'
                                  )
                              ),
    reservation_expires_at_ms INTEGER
                              CHECK (
                                  reservation_expires_at_ms IS NULL OR
                                  reservation_expires_at_ms BETWEEN -62135596800000 AND 253402300799999
                              ),
    mime_file_key             TEXT NOT NULL
                              CHECK (length(CAST(mime_file_key AS BLOB)) BETWEEN 1 AND 512),
    rfc_message_id            TEXT
                              CHECK (
                                  rfc_message_id IS NULL OR
                                  length(CAST(rfc_message_id AS BLOB)) BETWEEN 1 AND 998
                              ),
    envelope_from             TEXT NOT NULL
                              CHECK (length(CAST(envelope_from AS BLOB)) BETWEEN 1 AND 320),
    wire_byte_count           INTEGER
                              CHECK (
                                  wire_byte_count IS NULL OR
                                  wire_byte_count BETWEEN 1 AND 8388608
                              ),
    state                     TEXT NOT NULL
                              CHECK (state IN (
                                  'reserved', 'ready', 'in_flight', 'retry_wait',
                                  'uncertain', 'permanent_failure', 'delivered'
                              )),
    attempt_count             INTEGER NOT NULL DEFAULT 0
                              CHECK (attempt_count BETWEEN 0 AND 1000),
    not_before_ms             INTEGER
                              CHECK (
                                  not_before_ms IS NULL OR
                                  not_before_ms BETWEEN -62135596800000 AND 253402300799999
                              ),
    claim_epoch               INTEGER NOT NULL DEFAULT 0
                              CHECK (claim_epoch BETWEEN 0 AND 9223372036854775807),
    lease_expires_at_ms       INTEGER
                              CHECK (
                                  lease_expires_at_ms IS NULL OR
                                  lease_expires_at_ms BETWEEN -62135596800000 AND 253402300799999
                              ),
    delivery_started          INTEGER NOT NULL DEFAULT 0
                              CHECK (delivery_started IN (0, 1)),
    error_class               TEXT
                              CHECK (
                                  error_class IS NULL OR error_class IN (
                                      'network', 'rate_limit', 'authentication',
                                      'configuration', 'protocol', 'permanent', 'ambiguous'
                                  )
                              ),
    error_code                TEXT
                              CHECK (
                                  error_code IS NULL OR
                                  length(CAST(error_code AS BLOB)) BETWEEN 1 AND 64
                              ),
    created_at_ms             INTEGER NOT NULL
                              CHECK (
                                  created_at_ms BETWEEN -62135596800000 AND 253402300799999
                              ),
    updated_at_ms             INTEGER NOT NULL
                              CHECK (
                                  updated_at_ms BETWEEN -62135596800000 AND 253402300799999
                              ),
    delivered_at_ms           INTEGER
                              CHECK (
                                  delivered_at_ms IS NULL OR
                                  delivered_at_ms BETWEEN -62135596800000 AND 253402300799999
                              ),
    FOREIGN KEY (message_id, account_id)
        REFERENCES messages(id, account_id) ON DELETE CASCADE,
    UNIQUE (account_id, rfc_message_id),
    CHECK (
        (state = 'reserved') =
        (reservation_token IS NOT NULL AND reservation_expires_at_ms IS NOT NULL)
    ),
    CHECK ((state = 'in_flight') = (lease_expires_at_ms IS NOT NULL)),
    CHECK (state <> 'in_flight' OR (claim_epoch > 0 AND attempt_count > 0)),
    CHECK (delivery_started = 0 OR state IN ('in_flight', 'uncertain')),
    CHECK (
        state IN ('reserved', 'permanent_failure', 'uncertain') OR
        (wire_byte_count IS NOT NULL AND rfc_message_id IS NOT NULL)
    ),
    CHECK ((state = 'delivered') = (delivered_at_ms IS NOT NULL))
) STRICT;

CREATE TABLE outbox_recipients (
    message_id    INTEGER NOT NULL
                  REFERENCES outbox(message_id) ON DELETE CASCADE,
    kind          TEXT NOT NULL CHECK (kind IN ('to', 'cc', 'bcc')),
    ordinal       INTEGER NOT NULL CHECK (ordinal BETWEEN 0 AND 65535),
    address       TEXT NOT NULL
                  CHECK (length(CAST(address AS BLOB)) BETWEEN 1 AND 320),
    display_name  TEXT NOT NULL DEFAULT ''
                  CHECK (length(CAST(display_name AS BLOB)) <= 320),
    PRIMARY KEY (message_id, kind, ordinal)
) STRICT, WITHOUT ROWID;

INSERT INTO outbox (
    message_id, account_id, configuration_generation, artifact_generation,
    draft_revision, mime_file_key, envelope_from, wire_byte_count, state,
    attempt_count, not_before_ms, claim_epoch, delivery_started,
    error_class, error_code, created_at_ms, updated_at_ms
)
SELECT legacy.message_id, message.account_id, account.configuration_generation, 1,
       message.revision, legacy.mime_file_key, legacy.envelope_from,
       min(legacy.wire_byte_count, 8388608),
       CASE
           WHEN legacy.state IN ('in_flight', 'sending') THEN 'uncertain'
           ELSE 'permanent_failure'
       END,
       legacy.attempt_count, legacy.next_attempt_at_ms, 0,
       CASE WHEN legacy.state IN ('in_flight', 'sending') THEN 1 ELSE 0 END,
       CASE
           WHEN legacy.state IN ('in_flight', 'sending') THEN 'ambiguous'
           ELSE 'configuration'
       END,
       'legacy_unverified', 0, 0
FROM outbox_v11 AS legacy
JOIN messages AS message ON message.id = legacy.message_id
JOIN accounts AS account ON account.id = message.account_id;

INSERT INTO outbox_recipients (message_id, kind, ordinal, address, display_name)
SELECT recipient.message_id, recipient.kind, recipient.ordinal,
       recipient.address, recipient.display_name
FROM outbox_recipients_v11 AS recipient;

DROP TABLE outbox_recipients_v11;
DROP TABLE outbox_v11;

CREATE INDEX idx_outbox_pending
    ON outbox(account_id, state, not_before_ms, message_id);

CREATE INDEX idx_outbox_lease
    ON outbox(state, lease_expires_at_ms, message_id)
    WHERE state = 'in_flight';

CREATE INDEX idx_outbox_reservation
    ON outbox(state, reservation_expires_at_ms, message_id)
    WHERE state = 'reserved';

CREATE INDEX idx_outbox_mime_file
    ON outbox(mime_file_key);

CREATE TRIGGER enforce_outbox_total_limit
BEFORE INSERT ON outbox
WHEN (SELECT count(*) FROM outbox) >= 256
BEGIN
    SELECT RAISE(ABORT, 'outbox item limit exceeded');
END;

CREATE TRIGGER enforce_outbox_active_byte_limit_insert
BEFORE INSERT ON outbox
WHEN NEW.state IN ('reserved', 'ready', 'in_flight', 'retry_wait')
 AND coalesce((
         SELECT sum(wire_byte_count) FROM outbox
         WHERE state IN ('reserved', 'ready', 'in_flight', 'retry_wait')
     ), 0) + coalesce(NEW.wire_byte_count, 0) > 134217728
BEGIN
    SELECT RAISE(ABORT, 'active outbox byte limit exceeded');
END;

CREATE TRIGGER enforce_outbox_active_byte_limit_update
BEFORE UPDATE OF state, wire_byte_count ON outbox
WHEN NEW.state IN ('reserved', 'ready', 'in_flight', 'retry_wait')
 AND coalesce((
         SELECT sum(wire_byte_count) FROM outbox
         WHERE message_id <> OLD.message_id
           AND state IN ('reserved', 'ready', 'in_flight', 'retry_wait')
     ), 0) + coalesce(NEW.wire_byte_count, 0) > 134217728
BEGIN
    SELECT RAISE(ABORT, 'active outbox byte limit exceeded');
END;

CREATE TRIGGER enforce_outbox_active_count_insert
BEFORE INSERT ON outbox
WHEN NEW.state IN ('reserved', 'ready', 'in_flight', 'retry_wait')
 AND (
     SELECT count(*) FROM outbox
     WHERE state IN ('reserved', 'ready', 'in_flight', 'retry_wait')
 ) >= 128
BEGIN
    SELECT RAISE(ABORT, 'active outbox item limit exceeded');
END;

CREATE TRIGGER enforce_outbox_active_count_update
BEFORE UPDATE OF state ON outbox
WHEN OLD.state NOT IN ('reserved', 'ready', 'in_flight', 'retry_wait')
 AND NEW.state IN ('reserved', 'ready', 'in_flight', 'retry_wait')
 AND (
     SELECT count(*) FROM outbox
     WHERE state IN ('reserved', 'ready', 'in_flight', 'retry_wait')
 ) >= 128
BEGIN
    SELECT RAISE(ABORT, 'active outbox item limit exceeded');
END;

CREATE TRIGGER enforce_outbox_recipient_limit
BEFORE INSERT ON outbox_recipients
WHEN NOT EXISTS (
         SELECT 1 FROM outbox_recipients
         WHERE message_id = NEW.message_id
           AND kind = NEW.kind
           AND ordinal = NEW.ordinal
     )
 AND (
     SELECT count(*) FROM outbox_recipients
     WHERE message_id = NEW.message_id
 ) >= 64
BEGIN
    SELECT RAISE(ABORT, 'outbox recipient limit exceeded');
END;

CREATE TRIGGER dequeue_outbox_file_v12_insert
AFTER INSERT ON outbox
BEGIN
    DELETE FROM file_gc WHERE file_key = NEW.mime_file_key;
END;

CREATE TRIGGER dequeue_outbox_file_v12_update
AFTER UPDATE OF mime_file_key ON outbox
BEGIN
    DELETE FROM file_gc WHERE file_key = NEW.mime_file_key;
END;
