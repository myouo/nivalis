CREATE TABLE account_mailbox_stats (
    account_id      INTEGER PRIMARY KEY
                    REFERENCES accounts(id) ON DELETE CASCADE,
    inbox_total     INTEGER NOT NULL DEFAULT 0
                    CHECK (inbox_total BETWEEN 0 AND 9223372036854775807),
    inbox_unread    INTEGER NOT NULL DEFAULT 0
                    CHECK (inbox_unread BETWEEN 0 AND 9223372036854775807),
    starred_total   INTEGER NOT NULL DEFAULT 0
                    CHECK (starred_total BETWEEN 0 AND 9223372036854775807),
    sent_total      INTEGER NOT NULL DEFAULT 0
                    CHECK (sent_total BETWEEN 0 AND 9223372036854775807),
    drafts_total    INTEGER NOT NULL DEFAULT 0
                    CHECK (drafts_total BETWEEN 0 AND 9223372036854775807),
    archive_total   INTEGER NOT NULL DEFAULT 0
                    CHECK (archive_total BETWEEN 0 AND 9223372036854775807),
    trash_total     INTEGER NOT NULL DEFAULT 0
                    CHECK (trash_total BETWEEN 0 AND 9223372036854775807),
    dirty           INTEGER NOT NULL DEFAULT 0
                    CHECK (dirty IN (0, 1)),
    CHECK (inbox_unread <= inbox_total)
) STRICT;

WITH classified AS (
    SELECT
        m.id,
        m.account_id,
        m.unread,
        m.starred,
        count(mf.folder_id) <> 0 AS has_membership,
        max(CASE WHEN f.role = 'inbox' THEN 1 ELSE 0 END) AS has_inbox,
        max(CASE WHEN f.role = 'sent' THEN 1 ELSE 0 END) AS has_sent,
        max(CASE WHEN f.role = 'drafts' THEN 1 ELSE 0 END) AS has_drafts,
        max(CASE WHEN f.role = 'archive' THEN 1 ELSE 0 END) AS has_archive,
        max(CASE WHEN f.role = 'trash' THEN 1 ELSE 0 END) AS has_trash
    FROM messages AS m
    LEFT JOIN message_folders AS mf
      ON mf.message_id = m.id
     AND mf.account_id = m.account_id
    LEFT JOIN folders AS f
      ON f.id = mf.folder_id
     AND f.account_id = mf.account_id
    GROUP BY m.id
), totals AS (
    SELECT
        account_id,
        sum(CASE WHEN has_inbox AND NOT has_trash THEN 1 ELSE 0 END) AS inbox_total,
        sum(CASE WHEN unread AND has_inbox AND NOT has_trash THEN 1 ELSE 0 END) AS inbox_unread,
        sum(CASE WHEN starred AND has_membership AND NOT has_trash THEN 1 ELSE 0 END) AS starred_total,
        sum(CASE WHEN has_sent AND NOT has_trash THEN 1 ELSE 0 END) AS sent_total,
        sum(CASE WHEN has_drafts AND NOT has_trash THEN 1 ELSE 0 END) AS drafts_total,
        sum(CASE WHEN has_archive AND NOT has_trash THEN 1 ELSE 0 END) AS archive_total,
        sum(CASE WHEN has_trash THEN 1 ELSE 0 END) AS trash_total
    FROM classified
    GROUP BY account_id
)
INSERT INTO account_mailbox_stats (
    account_id,
    inbox_total,
    inbox_unread,
    starred_total,
    sent_total,
    drafts_total,
    archive_total,
    trash_total
)
SELECT
    a.id,
    coalesce(t.inbox_total, 0),
    coalesce(t.inbox_unread, 0),
    coalesce(t.starred_total, 0),
    coalesce(t.sent_total, 0),
    coalesce(t.drafts_total, 0),
    coalesce(t.archive_total, 0),
    coalesce(t.trash_total, 0)
FROM accounts AS a
LEFT JOIN totals AS t ON t.account_id = a.id;

CREATE TRIGGER initialize_account_mailbox_stats
AFTER INSERT ON accounts
BEGIN
    INSERT INTO account_mailbox_stats (account_id) VALUES (NEW.id);
END;

CREATE TRIGGER reject_account_limit_insert
BEFORE INSERT ON accounts
WHEN (SELECT count(*) FROM accounts) >= 64
 AND NOT EXISTS (
     SELECT 1 FROM accounts
     WHERE id = NEW.id OR (provider = NEW.provider AND remote_key = NEW.remote_key)
 )
BEGIN
    SELECT RAISE(ABORT, 'mail account limit exceeded');
END;

CREATE TRIGGER mark_mailbox_stats_dirty_message_insert
AFTER INSERT ON messages
BEGIN
    UPDATE account_mailbox_stats SET dirty = 1
    WHERE account_id = NEW.account_id AND dirty = 0;
END;

CREATE TRIGGER mark_mailbox_stats_dirty_message_delete
AFTER DELETE ON messages
BEGIN
    UPDATE account_mailbox_stats SET dirty = 1
    WHERE account_id = OLD.account_id AND dirty = 0;
END;

CREATE TRIGGER mark_mailbox_stats_dirty_message_flags
AFTER UPDATE OF account_id, unread, starred ON messages
BEGIN
    UPDATE account_mailbox_stats SET dirty = 1
    WHERE account_id IN (OLD.account_id, NEW.account_id) AND dirty = 0;
END;

CREATE TRIGGER mark_mailbox_stats_dirty_membership_insert
AFTER INSERT ON message_folders
BEGIN
    UPDATE account_mailbox_stats SET dirty = 1
    WHERE account_id = NEW.account_id AND dirty = 0;
END;

CREATE TRIGGER mark_mailbox_stats_dirty_membership_delete
AFTER DELETE ON message_folders
BEGIN
    UPDATE account_mailbox_stats SET dirty = 1
    WHERE account_id = OLD.account_id AND dirty = 0;
END;

CREATE TRIGGER mark_mailbox_stats_dirty_membership_update
AFTER UPDATE OF message_id, folder_id, account_id ON message_folders
BEGIN
    UPDATE account_mailbox_stats SET dirty = 1
    WHERE account_id IN (OLD.account_id, NEW.account_id) AND dirty = 0;
END;

CREATE TRIGGER mark_mailbox_stats_dirty_folder_role
AFTER UPDATE OF account_id, role ON folders
BEGIN
    UPDATE account_mailbox_stats SET dirty = 1
    WHERE account_id IN (OLD.account_id, NEW.account_id) AND dirty = 0;
END;

CREATE TRIGGER mark_mailbox_stats_dirty_folder_delete
BEFORE DELETE ON folders
WHEN EXISTS (
    SELECT 1 FROM message_folders WHERE folder_id = OLD.id AND account_id = OLD.account_id
)
BEGIN
    UPDATE account_mailbox_stats SET dirty = 1
    WHERE account_id = OLD.account_id AND dirty = 0;
END;
