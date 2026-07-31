import type {
  ImageResponse,
  ImageRow,
  ImageVariant,
  TagRow,
} from "./types";

export function publicAssetUrl(env: Env, path: string): string {
  const configuredOrigin = env.PUBLIC_IMAGE_ORIGIN?.trim().replace(/\/+$/u, "");
  if (!configuredOrigin) return path;
  try {
    const hostname = new URL(configuredOrigin).hostname;
    if (hostname === "img.example.com" || hostname.endsWith(".example.com")) {
      return path;
    }
  } catch {
    return path;
  }
  return `${configuredOrigin}${path}`;
}

export function imagePublicUrl(
  env: Env,
  image: Pick<ImageRow, "id" | "slug">,
  variant: ImageVariant,
): string {
  return publicAssetUrl(
    env,
    `/i/${image.id}/${variant}/${encodeURIComponent(image.slug)}`,
  );
}

export function toImageResponse(
  env: Env,
  image: ImageRow,
  tags: string[] = [],
): ImageResponse {
  const encodedSlug = encodeURIComponent(image.slug);
  const adminThumbnail = `/api/media/${image.id}/w320/${encodedSlug}`;
  const adminPreview = `/api/media/${image.id}/w640/${encodedSlug}`;
  const canUsePublicDelivery = image.visibility === "public";

  return {
    id: image.id,
    originalName: image.original_name,
    slug: image.slug,
    mimeType: image.mime_type,
    byteSize: image.byte_size,
    width: image.width,
    height: image.height,
    title: image.title,
    altText: image.alt_text,
    visibility: image.visibility,
    status: image.status,
    tags,
    createdAt: image.created_at,
    updatedAt: image.updated_at,
    deletedAt: image.deleted_at,
    urls: {
      original: imagePublicUrl(env, image, "original"),
      thumbnail: canUsePublicDelivery
        ? imagePublicUrl(env, image, "w320")
        : adminThumbnail,
      preview: canUsePublicDelivery
        ? imagePublicUrl(env, image, "w640")
        : adminPreview,
    },
  };
}

export async function getImageById(
  db: D1Database,
  id: string,
): Promise<ImageRow | null> {
  return db
    .prepare("SELECT * FROM images WHERE id = ?")
    .bind(id)
    .first<ImageRow>();
}

export async function getOwnedImageById(
  db: D1Database,
  id: string,
  ownerId: string,
): Promise<ImageRow | null> {
  return db
    .prepare("SELECT * FROM images WHERE id = ? AND owner_id = ?")
    .bind(id, ownerId)
    .first<ImageRow>();
}

export async function getTagsForImages(
  db: D1Database,
  imageIds: string[],
): Promise<Map<string, string[]>> {
  const tagsByImage = new Map<string, string[]>();
  if (imageIds.length === 0) {
    return tagsByImage;
  }

  const placeholders = imageIds.map(() => "?").join(", ");
  const result = await db
    .prepare(
      `SELECT it.image_id, t.name
       FROM image_tags it
       JOIN tags t ON t.id = it.tag_id
       WHERE it.image_id IN (${placeholders})
       ORDER BY t.normalized_name ASC`,
    )
    .bind(...imageIds)
    .all<{ image_id: string; name: string }>();

  for (const row of result.results) {
    const tags = tagsByImage.get(row.image_id) ?? [];
    tags.push(row.name);
    tagsByImage.set(row.image_id, tags);
  }
  return tagsByImage;
}

export async function getAllTags(
  db: D1Database,
  ownerId: string,
): Promise<TagRow[]> {
  const result = await db
    .prepare(
      `SELECT t.*, COUNT(it.image_id) AS usage_count
       FROM tags t
       JOIN image_tags it ON it.tag_id = t.id
       JOIN images i ON i.id = it.image_id
       WHERE i.owner_id = ?
       GROUP BY t.id
       ORDER BY t.normalized_name ASC`,
    )
    .bind(ownerId)
    .all<TagRow>();
  return result.results;
}
