ALTER TABLE message_content
    ADD COLUMN render_version INTEGER NOT NULL DEFAULT 2
        CHECK (render_version BETWEEN 1 AND 2);

-- Rows written by older builds only contain the plain-text projection of an
-- HTML body. Mark them for a one-time, on-demand refresh; future inserts use
-- the column default and are immediately renderable.
UPDATE message_content SET render_version = 1;
