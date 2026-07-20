WITH classified AS (
    SELECT m.account_id,
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
    SELECT account_id,
           sum(has_inbox AND NOT has_trash) AS inbox_total,
           sum(unread AND has_inbox AND NOT has_trash) AS inbox_unread,
           sum(starred AND has_membership AND NOT has_trash) AS starred_total,
           sum(has_sent AND NOT has_trash) AS sent_total,
           sum(has_drafts AND NOT has_trash) AS drafts_total,
           sum(has_archive AND NOT has_trash) AS archive_total,
           sum(has_trash) AS trash_total
      FROM classified
     GROUP BY account_id
)
UPDATE account_mailbox_stats
   SET (inbox_total, inbox_unread, starred_total, sent_total,
        drafts_total, archive_total, trash_total, dirty) = (
       SELECT coalesce(max(inbox_total), 0),
              coalesce(max(inbox_unread), 0),
              coalesce(max(starred_total), 0),
              coalesce(max(sent_total), 0),
              coalesce(max(drafts_total), 0),
              coalesce(max(archive_total), 0),
              coalesce(max(trash_total), 0),
              0
         FROM totals
        WHERE totals.account_id = account_mailbox_stats.account_id
   )
 WHERE dirty = 1;
