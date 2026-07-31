PRAGMA foreign_keys = ON;

CREATE TABLE images (
  id TEXT PRIMARY KEY,
  object_key TEXT NOT NULL UNIQUE,
  sha256 TEXT NOT NULL UNIQUE,
  original_name TEXT NOT NULL,
  slug TEXT NOT NULL,
  mime_type TEXT NOT NULL,
  byte_size INTEGER NOT NULL,
  width INTEGER,
  height INTEGER,
  title TEXT NOT NULL DEFAULT '',
  alt_text TEXT NOT NULL DEFAULT '',
  visibility TEXT NOT NULL DEFAULT 'private'
    CHECK (visibility IN ('public', 'private')),
  status TEXT NOT NULL DEFAULT 'uploading'
    CHECK (status IN ('uploading', 'active', 'trashed')),
  share_version INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER
);

CREATE INDEX idx_images_status_created
  ON images(status, created_at DESC);
CREATE INDEX idx_images_visibility
  ON images(visibility, status);
CREATE INDEX idx_images_mime_type
  ON images(mime_type, status);
CREATE INDEX idx_images_deleted_at
  ON images(deleted_at)
  WHERE deleted_at IS NOT NULL;

CREATE TABLE tags (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  normalized_name TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL
);

CREATE TABLE image_tags (
  image_id TEXT NOT NULL REFERENCES images(id) ON DELETE CASCADE,
  tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  PRIMARY KEY (image_id, tag_id)
);

CREATE INDEX idx_image_tags_tag_id ON image_tags(tag_id, image_id);
