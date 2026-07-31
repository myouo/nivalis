PRAGMA defer_foreign_keys = ON;

CREATE TABLE images_v2 (
  id TEXT PRIMARY KEY,
  owner_id TEXT NOT NULL,
  object_key TEXT NOT NULL UNIQUE,
  sha256 TEXT NOT NULL,
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
  deleted_at INTEGER,
  UNIQUE(owner_id, sha256)
);

INSERT INTO images_v2 (
  id, owner_id, object_key, sha256, original_name, slug, mime_type, byte_size,
  width, height, title, alt_text, visibility, status, share_version,
  created_at, updated_at, deleted_at
)
SELECT
  id, 'legacy', object_key, sha256, original_name, slug, mime_type, byte_size,
  width, height, title, alt_text, visibility, status, share_version,
  created_at, updated_at, deleted_at
FROM images;

CREATE TABLE image_tags_v2 (
  image_id TEXT NOT NULL REFERENCES images_v2(id) ON DELETE CASCADE,
  tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  PRIMARY KEY (image_id, tag_id)
);

INSERT INTO image_tags_v2 (image_id, tag_id)
SELECT image_id, tag_id FROM image_tags;

DROP TABLE image_tags;
DROP TABLE images;

ALTER TABLE images_v2 RENAME TO images;
ALTER TABLE image_tags_v2 RENAME TO image_tags;

CREATE INDEX idx_images_owner_status_created
  ON images(owner_id, status, created_at DESC);
CREATE INDEX idx_images_visibility
  ON images(owner_id, visibility, status);
CREATE INDEX idx_images_mime_type
  ON images(owner_id, mime_type, status);
CREATE INDEX idx_images_deleted_at
  ON images(deleted_at)
  WHERE deleted_at IS NOT NULL;
CREATE INDEX idx_image_tags_tag_id ON image_tags(tag_id, image_id);
