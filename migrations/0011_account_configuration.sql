ALTER TABLE accounts
ADD COLUMN configuration_generation INTEGER NOT NULL DEFAULT 1
CHECK (configuration_generation BETWEEN 1 AND 9223372036854775807);

CREATE TABLE account_connections (
    account_id          INTEGER PRIMARY KEY
                        REFERENCES accounts(id) ON DELETE CASCADE,
    credential_key      TEXT NOT NULL UNIQUE
                        CHECK (
                            length(CAST(credential_key AS BLOB)) = 32 AND
                            credential_key NOT GLOB '*[^0-9a-f]*'
                        ),
    auth_kind           TEXT NOT NULL
                        CHECK (auth_kind IN ('app_password', 'oauth2')),
    login_name          TEXT NOT NULL
                        CHECK (length(CAST(login_name AS BLOB)) BETWEEN 1 AND 320),
    imap_host           TEXT NOT NULL
                        CHECK (length(CAST(imap_host AS BLOB)) BETWEEN 1 AND 253),
    imap_port           INTEGER NOT NULL
                        CHECK (imap_port BETWEEN 1 AND 65535),
    diagnostic_generation INTEGER NOT NULL DEFAULT 0
                        CHECK (diagnostic_generation BETWEEN 0 AND 9223372036854775807),
    diagnostic_state    TEXT NOT NULL DEFAULT 'never'
                        CHECK (
                            diagnostic_state IN (
                                'never', 'ready', 'authentication', 'permission',
                                'certificate', 'timeout', 'offline', 'protocol'
                            )
                        ),
    last_checked_at_ms  INTEGER
                        CHECK (
                            last_checked_at_ms IS NULL OR
                            last_checked_at_ms BETWEEN -62135596800000 AND 253402300799999
                        ),
    CHECK (
        (diagnostic_state = 'never' AND last_checked_at_ms IS NULL) OR
        (diagnostic_state <> 'never' AND last_checked_at_ms IS NOT NULL)
    )
) STRICT;
