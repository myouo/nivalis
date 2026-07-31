import type { ImageRow } from "./types";

const DAY = 24 * 60 * 60 * 1000;

export async function cleanupExpiredImages(env: Env): Promise<void> {
  const now = Date.now();
  const result = await env.DB.prepare(
    `SELECT * FROM images
     WHERE (status = 'uploading' AND created_at < ?)
        OR (status = 'trashed' AND deleted_at < ?)
     LIMIT 100`,
  )
    .bind(now - DAY, now - 30 * DAY)
    .all<ImageRow>();

  if (result.results.length === 0) {
    return;
  }

  await env.IMAGES_BUCKET.delete(result.results.map((image) => image.object_key));
  await env.DB.batch(
    result.results.map((image) =>
      env.DB.prepare("DELETE FROM images WHERE id = ?").bind(image.id),
    ),
  );
  await env.DB.prepare(
    "DELETE FROM tags WHERE NOT EXISTS (SELECT 1 FROM image_tags WHERE tag_id = tags.id)",
  ).run();

  console.log(
    JSON.stringify({
      message: "expired images cleaned",
      count: result.results.length,
    }),
  );
}
