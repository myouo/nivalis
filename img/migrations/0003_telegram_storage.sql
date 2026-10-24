ALTER TABLE images ADD COLUMN storage_backend TEXT NOT NULL DEFAULT 'r2'
  CHECK (storage_backend IN ('r2', 'telegram'));
ALTER TABLE images ADD COLUMN upload_started_at INTEGER;

-- References survive logical deletion so old Telegram messages can be audited
-- and removed manually when the Bot API's 48-hour deletion window has passed.
CREATE TABLE telegram_objects (
  image_id TEXT PRIMARY KEY,
  file_id TEXT NOT NULL,
  file_unique_id TEXT NOT NULL,
  message_id INTEGER NOT NULL,
  chat_id TEXT NOT NULL,
  bot_id TEXT NOT NULL,
  uploaded_at INTEGER NOT NULL,
  verified_at INTEGER,
  r2_backup INTEGER NOT NULL DEFAULT 0 CHECK (r2_backup IN (0, 1)),
  deleted_at INTEGER,
  cleanup_error TEXT
);

CREATE INDEX idx_telegram_objects_deleted ON telegram_objects(deleted_at)
  WHERE deleted_at IS NOT NULL;
CREATE INDEX idx_images_storage_migration ON images(storage_backend, status, id);

-- Reserve send slots atomically across Worker instances sharing one channel.
CREATE TABLE telegram_send_slots (
  chat_id TEXT PRIMARY KEY,
  next_send_at INTEGER NOT NULL
);
