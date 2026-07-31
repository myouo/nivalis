ALTER TABLE account_mailbox_stats ADD COLUMN remote_inbox_total INTEGER
    CHECK (remote_inbox_total IS NULL OR remote_inbox_total BETWEEN 0 AND 4294967295);

ALTER TABLE account_mailbox_stats ADD COLUMN remote_inbox_total_updated_at_ms INTEGER
    CHECK (
        remote_inbox_total_updated_at_ms IS NULL OR
        remote_inbox_total_updated_at_ms BETWEEN -62135596800000 AND 253402300799999
    );

