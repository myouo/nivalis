import { Hono } from "hono";
import { z } from "zod";
import { purgeImageCache } from "./cache";
import {
  createShareSignature,
  createUlid,
  decodeCursor,
  encodeCursor,
} from "./crypto";
import {
  getAllTags,
  getOwnedImageById,
  getTagsForImages,
  publicAssetUrl,
  toImageResponse,
} from "./db";
import {
  IMAGE_MIME_TYPES,
  IMAGE_VARIANTS,
  isImageMimeType,
  type AppBindings,
  type ImageMimeType,
  type ImageRow,
  type ImageVariant,
  type ShareFormat,
} from "./types";

const api = new Hono<AppBindings>();
const DAY_SECONDS = 24 * 60 * 60;

const uploadSchema = z.object({
  name: z.string().trim().min(1).max(200),
  type: z.enum(IMAGE_MIME_TYPES),
  size: z.number().int().positive(),
  sha256: z.string().regex(/^[a-f0-9]{64}$/u),
  visibility: z.enum(["public", "private"]).default("private"),
});

const updateSchema = z
  .object({
    title: z.string().trim().max(200).optional(),
    altText: z.string().trim().max(500).optional(),
    visibility: z.enum(["public", "private"]).optional(),
  })
  .refine((value) => Object.keys(value).length > 0);

const tagsSchema = z.object({
  tags: z.array(z.string().trim().min(1).max(32)).max(12),
});

const shareSchema = z.object({
  expiresIn: z.union([
    z.literal(3600),
    z.literal(DAY_SECONDS),
    z.literal(7 * DAY_SECONDS),
    z.literal(30 * DAY_SECONDS),
  ]),
  variant: z
    .enum(Object.keys(IMAGE_VARIANTS) as [ImageVariant, ...ImageVariant[]])
    .default("original"),
  format: z.enum(["original", "webp"] satisfies ShareFormat[]).default("original"),
  quality: z.number().int().min(1).max(100).default(85),
});

function slugifyFilename(filename: string): string {
  const normalized = filename
    .normalize("NFKC")
    .replace(/[^\p{Letter}\p{Number}._-]+/gu, "-")
    .replace(/-{2,}/gu, "-")
    .replace(/^-|-$/gu, "")
    .toLocaleLowerCase();
  return normalized || "image";
}

function extensionForMime(mime: ImageMimeType): string {
  return {
    "image/jpeg": "jpg",
    "image/png": "png",
    "image/webp": "webp",
    "image/avif": "avif",
    "image/gif": "gif",
  }[mime];
}

function uploadObjectKey(id: string, mime: ImageMimeType, now: Date): string {
  const year = String(now.getUTCFullYear());
  const month = String(now.getUTCMonth() + 1).padStart(2, "0");
  return `originals/${year}/${month}/${id}.${extensionForMime(mime)}`;
}

function normalizeTag(tag: string): string {
  return tag.normalize("NFKC").trim().toLocaleLowerCase();
}

function clientAddress(request: Request): string {
  return (
    request.headers.get("CF-Connecting-IP") ??
    request.headers.get("X-Forwarded-For")?.split(",")[0]?.trim() ??
    "unknown"
  );
}

api.post("/uploads", async (context) => {
  const body: unknown = await context.req.json().catch(() => null);
  const parsed = uploadSchema.safeParse(body);
  if (!parsed.success) {
    return context.json(
      {
        code: "INVALID_UPLOAD",
        message: "图片信息不完整或格式不正确。",
        requestId: context.get("requestId"),
      },
      400,
    );
  }

  const maxBytes = Number(context.env.MAX_UPLOAD_BYTES);
  if (parsed.data.size > maxBytes) {
    return context.json(
      {
        code: "FILE_TOO_LARGE",
        message: `单张图片不能超过 ${Math.floor(maxBytes / 1024 / 1024)} MB。`,
        requestId: context.get("requestId"),
      },
      413,
    );
  }

  const ownerId = context.get("ownerId");
  const [ownerLimit, addressLimit] = await Promise.all([
    context.env.UPLOAD_USER_RATE_LIMITER.limit({ key: ownerId }),
    context.env.UPLOAD_IP_RATE_LIMITER.limit({
      key: clientAddress(context.req.raw),
    }),
  ]);
  if (!ownerLimit.success || !addressLimit.success) {
    return context.json(
      {
        code: "UPLOAD_RATE_LIMITED",
        message: "上传过于频繁，请稍后再试。",
        requestId: context.get("requestId"),
      },
      429,
    );
  }

  const existing = await context.env.DB.prepare(
    "SELECT * FROM images WHERE owner_id = ? AND sha256 = ?",
  )
    .bind(ownerId, parsed.data.sha256)
    .first<ImageRow>();
  if (existing) {
    if (existing.status === "active") {
      const tags = await getTagsForImages(context.env.DB, [existing.id]);
      return context.json({
        duplicate: true,
        image: toImageResponse(
          context.env,
          existing,
          tags.get(existing.id) ?? [],
        ),
      });
    }
    if (existing.status === "trashed") {
      const updatedAt = Date.now();
      await context.env.DB.prepare(
        `UPDATE images
         SET status = 'active', visibility = ?, deleted_at = NULL,
             share_version = share_version + 1, updated_at = ?
         WHERE id = ?`,
      )
        .bind(parsed.data.visibility, updatedAt, existing.id)
        .run();
      const restored = await getOwnedImageById(
        context.env.DB,
        existing.id,
        ownerId,
      );
      return context.json({
        duplicate: true,
        restored: true,
        image: restored ? toImageResponse(context.env, restored) : null,
      });
    }

    if (existing.created_at > Date.now() - DAY_SECONDS * 1000) {
      return context.json(
        {
          code: "UPLOAD_IN_PROGRESS",
          message: "相同图片已有一个上传任务正在进行。",
          requestId: context.get("requestId"),
        },
        409,
      );
    }
    await context.env.IMAGES_BUCKET.delete(existing.object_key);
    await context.env.DB.prepare("DELETE FROM images WHERE id = ?")
      .bind(existing.id)
      .run();
  }

  const quota = await context.env.DB.prepare(
    `SELECT COUNT(*) AS image_count, COALESCE(SUM(byte_size), 0) AS reserved_bytes
     FROM images
     WHERE owner_id = ?`,
  )
    .bind(ownerId)
    .first<{ image_count: number; reserved_bytes: number }>();
  const maxOwnerImages = Number(context.env.MAX_OWNER_IMAGES);
  const maxOwnerStorageBytes = Number(context.env.MAX_OWNER_STORAGE_BYTES);
  if ((quota?.image_count ?? 0) >= maxOwnerImages) {
    return context.json(
      {
        code: "IMAGE_QUOTA_EXCEEDED",
        message: `当前浏览器最多可保存 ${maxOwnerImages} 张图片。`,
        requestId: context.get("requestId"),
      },
      413,
    );
  }
  if (
    (quota?.reserved_bytes ?? 0) + parsed.data.size >
    maxOwnerStorageBytes
  ) {
    return context.json(
      {
        code: "STORAGE_QUOTA_EXCEEDED",
        message: "当前浏览器的图片存储空间已用完。",
        requestId: context.get("requestId"),
      },
      413,
    );
  }

  const now = new Date();
  const timestamp = now.getTime();
  const id = createUlid(timestamp);
  const slug = slugifyFilename(parsed.data.name);
  const objectKey = uploadObjectKey(id, parsed.data.type, now);
  await context.env.DB.prepare(
    `INSERT INTO images (
       id, owner_id, object_key, sha256, original_name, slug, mime_type, byte_size,
       visibility, status, created_at, updated_at
     ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'uploading', ?, ?)`,
  )
    .bind(
      id,
      ownerId,
      objectKey,
      parsed.data.sha256,
      parsed.data.name,
      slug,
      parsed.data.type,
      parsed.data.size,
      parsed.data.visibility,
      timestamp,
      timestamp,
    )
    .run();

  return context.json(
    {
      duplicate: false,
      uploadId: id,
      putUrl: `/api/uploads/${id}`,
    },
    201,
  );
});

api.put("/uploads/:id", async (context) => {
  const image = await getOwnedImageById(
    context.env.DB,
    context.req.param("id"),
    context.get("ownerId"),
  );
  if (!image) {
    return context.json(
      {
        code: "UPLOAD_NOT_FOUND",
        message: "上传任务不存在或已过期。",
        requestId: context.get("requestId"),
      },
      404,
    );
  }
  if (image.status === "active") {
    return context.json({ image: toImageResponse(context.env, image) });
  }
  if (image.status !== "uploading") {
    return context.json(
      {
        code: "UPLOAD_CLOSED",
        message: "这个上传任务已经关闭。",
        requestId: context.get("requestId"),
      },
      409,
    );
  }

  const contentType = context.req.header("Content-Type")?.split(";")[0] ?? "";
  const contentLength = Number(context.req.header("Content-Length"));
  if (
    !isImageMimeType(contentType) ||
    contentType !== image.mime_type ||
    (Number.isFinite(contentLength) && contentLength > 0 && contentLength !== image.byte_size) ||
    !context.req.raw.body
  ) {
    return context.json(
      {
        code: "UPLOAD_MISMATCH",
        message: "上传内容与创建任务时的图片信息不一致。",
        requestId: context.get("requestId"),
      },
      400,
    );
  }

  try {
    await context.env.IMAGES_BUCKET.put(
      image.object_key,
      context.req.raw.body,
      {
        httpMetadata: {
          contentType: image.mime_type,
          contentDisposition: `inline; filename="${image.slug}"`,
          cacheControl: "public, max-age=31536000, immutable",
        },
        customMetadata: {
          originalName: image.original_name,
          sha256: image.sha256,
        },
      },
    );

    let width: number | null = null;
    let height: number | null = null;
    if (image.byte_size <= 20 * 1024 * 1024) {
      const stored = await context.env.IMAGES_BUCKET.get(image.object_key);
      if (stored) {
        try {
          const info = await context.env.IMAGES.info(stored.body);
          if ("width" in info) {
            width = info.width;
            height = info.height;
          }
        } catch (error) {
          console.warn(
            JSON.stringify({
              message: "image metadata inspection skipped",
              imageId: image.id,
              error: error instanceof Error ? error.message : String(error),
            }),
          );
        }
      }
    }

    const updatedAt = Date.now();
    await context.env.DB.prepare(
      `UPDATE images
       SET status = 'active', width = ?, height = ?, updated_at = ?
       WHERE id = ? AND status = 'uploading'`,
    )
      .bind(width, height, updatedAt, image.id)
      .run();
    const uploaded = await getOwnedImageById(
      context.env.DB,
      image.id,
      context.get("ownerId"),
    );
    if (!uploaded) {
      throw new Error("Image record disappeared during upload");
    }
    console.log(
      JSON.stringify({
        message: "image uploaded",
        requestId: context.get("requestId"),
        imageId: image.id,
        bytes: image.byte_size,
        owner: context.get("ownerId").slice(0, 12),
      }),
    );
    return context.json(
      { image: toImageResponse(context.env, uploaded) },
      201,
    );
  } catch (error) {
    await context.env.IMAGES_BUCKET.delete(image.object_key);
    await context.env.DB.prepare("DELETE FROM images WHERE id = ?")
      .bind(image.id)
      .run();
    throw error;
  }
});

api.get("/images", async (context) => {
  const limit = Math.min(Math.max(Number(context.req.query("limit")) || 40, 1), 60);
  const status = context.req.query("status") === "trashed" ? "trashed" : "active";
  const visibility = context.req.query("visibility");
  const type = context.req.query("type");
  const tag = context.req.query("tag")?.trim();
  const search = context.req.query("q")?.trim();
  const cursorValue = context.req.query("cursor");
  const cursor = cursorValue ? decodeCursor(cursorValue) : null;
  if (cursorValue && !cursor) {
    return context.json(
      {
        code: "INVALID_CURSOR",
        message: "分页位置无效，请刷新图库。",
        requestId: context.get("requestId"),
      },
      400,
    );
  }

  const clauses = ["i.owner_id = ?", "i.status = ?"];
  const bindings: unknown[] = [context.get("ownerId"), status];
  if (visibility === "public" || visibility === "private") {
    clauses.push("i.visibility = ?");
    bindings.push(visibility);
  }
  if (type && isImageMimeType(type)) {
    clauses.push("i.mime_type = ?");
    bindings.push(type);
  }
  if (tag) {
    clauses.push(
      "EXISTS (SELECT 1 FROM image_tags fit JOIN tags ft ON ft.id = fit.tag_id WHERE fit.image_id = i.id AND ft.normalized_name = ?)",
    );
    bindings.push(normalizeTag(tag));
  }
  if (search) {
    clauses.push(
      `(i.original_name LIKE ? OR i.title LIKE ? OR i.alt_text LIKE ?
        OR EXISTS (
          SELECT 1 FROM image_tags sit
          JOIN tags st ON st.id = sit.tag_id
          WHERE sit.image_id = i.id AND st.name LIKE ?
        ))`,
    );
    const pattern = `%${search}%`;
    bindings.push(pattern, pattern, pattern, pattern);
  }
  if (cursor) {
    clauses.push("(i.created_at < ? OR (i.created_at = ? AND i.id < ?))");
    bindings.push(cursor.createdAt, cursor.createdAt, cursor.id);
  }
  bindings.push(limit + 1);

  const result = await context.env.DB.prepare(
    `SELECT i.* FROM images i
     WHERE ${clauses.join(" AND ")}
     ORDER BY i.created_at DESC, i.id DESC
     LIMIT ?`,
  )
    .bind(...bindings)
    .all<ImageRow>();
  const hasMore = result.results.length > limit;
  const rows = hasMore ? result.results.slice(0, limit) : result.results;
  const tags = await getTagsForImages(
    context.env.DB,
    rows.map((image) => image.id),
  );
  const last = rows.at(-1);

  return context.json({
    items: rows.map((image) =>
      toImageResponse(context.env, image, tags.get(image.id) ?? []),
    ),
    nextCursor:
      hasMore && last ? encodeCursor(last.created_at, last.id) : null,
  });
});

api.get("/images/:id", async (context) => {
  const image = await getOwnedImageById(
    context.env.DB,
    context.req.param("id"),
    context.get("ownerId"),
  );
  if (!image) {
    return context.json(
      {
        code: "IMAGE_NOT_FOUND",
        message: "图片不存在。",
        requestId: context.get("requestId"),
      },
      404,
    );
  }
  const tags = await getTagsForImages(context.env.DB, [image.id]);
  return context.json({
    image: toImageResponse(context.env, image, tags.get(image.id) ?? []),
  });
});

api.patch("/images/:id", async (context) => {
  const body: unknown = await context.req.json().catch(() => null);
  const parsed = updateSchema.safeParse(body);
  if (!parsed.success) {
    return context.json(
      {
        code: "INVALID_UPDATE",
        message: "图片信息不符合要求。",
        requestId: context.get("requestId"),
      },
      400,
    );
  }
  const image = await getOwnedImageById(
    context.env.DB,
    context.req.param("id"),
    context.get("ownerId"),
  );
  if (!image || image.status !== "active") {
    return context.json(
      {
        code: "IMAGE_NOT_FOUND",
        message: "图片不存在。",
        requestId: context.get("requestId"),
      },
      404,
    );
  }

  const title = parsed.data.title ?? image.title;
  const altText = parsed.data.altText ?? image.alt_text;
  const visibility = parsed.data.visibility ?? image.visibility;
  const makingPrivate =
    image.visibility === "public" && visibility === "private";
  await context.env.DB.prepare(
    `UPDATE images
     SET title = ?, alt_text = ?, visibility = ?, updated_at = ?
     WHERE id = ?`,
  )
    .bind(title, altText, visibility, Date.now(), image.id)
    .run();

  if (makingPrivate) {
    try {
      await purgeImageCache(context.req.raw, context.env, image);
    } catch (error) {
      await context.env.DB.prepare(
        "UPDATE images SET visibility = 'public', updated_at = ? WHERE id = ?",
      )
        .bind(Date.now(), image.id)
        .run();
      throw error;
    }
  }
  const updated = await getOwnedImageById(
    context.env.DB,
    image.id,
    context.get("ownerId"),
  );
  const tags = await getTagsForImages(context.env.DB, [image.id]);
  return context.json({
    image: updated
      ? toImageResponse(context.env, updated, tags.get(image.id) ?? [])
      : null,
  });
});

api.put("/images/:id/tags", async (context) => {
  const body: unknown = await context.req.json().catch(() => null);
  const parsed = tagsSchema.safeParse(body);
  if (!parsed.success) {
    return context.json(
      {
        code: "INVALID_TAGS",
        message: "最多可添加 12 个标签，每个标签不超过 32 个字符。",
        requestId: context.get("requestId"),
      },
      400,
    );
  }
  const image = await getOwnedImageById(
    context.env.DB,
    context.req.param("id"),
    context.get("ownerId"),
  );
  if (!image || image.status !== "active") {
    return context.json(
      {
        code: "IMAGE_NOT_FOUND",
        message: "图片不存在。",
        requestId: context.get("requestId"),
      },
      404,
    );
  }

  const uniqueTags = new Map<string, string>();
  for (const name of parsed.data.tags) {
    uniqueTags.set(normalizeTag(name), name.trim());
  }
  const now = Date.now();
  const statements: D1PreparedStatement[] = [];
  for (const [normalized, name] of uniqueTags) {
    statements.push(
      context.env.DB.prepare(
        "INSERT OR IGNORE INTO tags (id, name, normalized_name, created_at) VALUES (?, ?, ?, ?)",
      ).bind(createUlid(now), name, normalized, now),
    );
  }
  statements.push(
    context.env.DB.prepare("DELETE FROM image_tags WHERE image_id = ?").bind(
      image.id,
    ),
  );
  for (const normalized of uniqueTags.keys()) {
    statements.push(
      context.env.DB.prepare(
        `INSERT INTO image_tags (image_id, tag_id)
         SELECT ?, id FROM tags WHERE normalized_name = ?`,
      ).bind(image.id, normalized),
    );
  }
  statements.push(
    context.env.DB.prepare("UPDATE images SET updated_at = ? WHERE id = ?").bind(
      now,
      image.id,
    ),
  );
  await context.env.DB.batch(statements);
  await context.env.DB.prepare(
    "DELETE FROM tags WHERE NOT EXISTS (SELECT 1 FROM image_tags WHERE tag_id = tags.id)",
  ).run();

  const tags = await getTagsForImages(context.env.DB, [image.id]);
  const updated = await getOwnedImageById(
    context.env.DB,
    image.id,
    context.get("ownerId"),
  );
  return context.json({
    image: updated
      ? toImageResponse(context.env, updated, tags.get(image.id) ?? [])
      : null,
  });
});

api.post("/images/:id/trash", async (context) => {
  const image = await getOwnedImageById(
    context.env.DB,
    context.req.param("id"),
    context.get("ownerId"),
  );
  if (!image || image.status !== "active") {
    return context.json(
      {
        code: "IMAGE_NOT_FOUND",
        message: "图片不存在。",
        requestId: context.get("requestId"),
      },
      404,
    );
  }
  const now = Date.now();
  await context.env.DB.prepare(
    `UPDATE images
     SET status = 'trashed', deleted_at = ?, updated_at = ?,
         share_version = share_version + 1
     WHERE id = ?`,
  )
    .bind(now, now, image.id)
    .run();
  try {
    await purgeImageCache(context.req.raw, context.env, image);
  } catch (error) {
    await context.env.DB.prepare(
      `UPDATE images
       SET status = 'active', deleted_at = NULL, updated_at = ?
       WHERE id = ?`,
    )
      .bind(Date.now(), image.id)
      .run();
    throw error;
  }
  return context.json({ ok: true });
});

api.post("/images/:id/restore", async (context) => {
  const image = await getOwnedImageById(
    context.env.DB,
    context.req.param("id"),
    context.get("ownerId"),
  );
  if (!image || image.status !== "trashed") {
    return context.json(
      {
        code: "IMAGE_NOT_FOUND",
        message: "回收站中没有这张图片。",
        requestId: context.get("requestId"),
      },
      404,
    );
  }
  await context.env.DB.prepare(
    `UPDATE images
     SET status = 'active', visibility = 'private', deleted_at = NULL,
         updated_at = ?, share_version = share_version + 1
     WHERE id = ?`,
  )
    .bind(Date.now(), image.id)
    .run();
  const restored = await getOwnedImageById(
    context.env.DB,
    image.id,
    context.get("ownerId"),
  );
  return context.json({
    image: restored ? toImageResponse(context.env, restored) : null,
  });
});

api.delete("/images/:id", async (context) => {
  const image = await getOwnedImageById(
    context.env.DB,
    context.req.param("id"),
    context.get("ownerId"),
  );
  if (!image || image.status !== "trashed") {
    return context.json(
      {
        code: "DELETE_REQUIRES_TRASH",
        message: "图片必须先移入回收站。",
        requestId: context.get("requestId"),
      },
      409,
    );
  }
  await purgeImageCache(context.req.raw, context.env, image);
  await context.env.IMAGES_BUCKET.delete(image.object_key);
  await context.env.DB.prepare("DELETE FROM images WHERE id = ?")
    .bind(image.id)
    .run();
  return context.json({ ok: true });
});

api.post("/images/:id/share-links", async (context) => {
  const body: unknown = await context.req.json().catch(() => null);
  const parsed = shareSchema.safeParse(body);
  if (!parsed.success) {
    return context.json(
      {
        code: "INVALID_EXPIRY",
        message: "请选择有效的链接期限。",
        requestId: context.get("requestId"),
      },
      400,
    );
  }
  const image = await getOwnedImageById(
    context.env.DB,
    context.req.param("id"),
    context.get("ownerId"),
  );
  if (
    !image ||
    image.status !== "active" ||
    image.visibility !== "private"
  ) {
    return context.json(
      {
        code: "PRIVATE_IMAGE_REQUIRED",
        message: "只有私密图片需要生成时效链接。",
        requestId: context.get("requestId"),
      },
      409,
    );
  }

  const expiresAt = Math.floor(Date.now() / 1000) + parsed.data.expiresIn;
  const shareVariant =
    parsed.data.format === "webp" ? "w1920" : parsed.data.variant;
  const signature = await createShareSignature(
    context.env.SHARE_SIGNING_KEY,
    image.id,
    shareVariant,
    expiresAt,
    image.share_version,
    parsed.data.format,
    parsed.data.quality,
  );
  const path = `/s/${image.id}/${shareVariant}/${encodeURIComponent(image.slug)}`;
  const search = new URLSearchParams({
    exp: String(expiresAt),
    v: String(image.share_version),
    sig: signature,
  });
  if (parsed.data.format === "webp") {
    search.set("format", "webp");
    search.set("q", String(parsed.data.quality));
  }
  return context.json({
    url: `${publicAssetUrl(context.env, path)}?${search.toString()}`,
    expiresAt,
  });
});

api.post("/images/:id/revoke-links", async (context) => {
  const image = await getOwnedImageById(
    context.env.DB,
    context.req.param("id"),
    context.get("ownerId"),
  );
  if (!image) {
    return context.json(
      {
        code: "IMAGE_NOT_FOUND",
        message: "图片不存在。",
        requestId: context.get("requestId"),
      },
      404,
    );
  }
  await context.env.DB.prepare(
    "UPDATE images SET share_version = share_version + 1, updated_at = ? WHERE id = ?",
  )
    .bind(Date.now(), image.id)
    .run();
  return context.json({ ok: true });
});

api.get("/tags", async (context) => {
  const tags = await getAllTags(context.env.DB, context.get("ownerId"));
  return context.json({
    items: tags.map((tag) => ({ id: tag.id, name: tag.name })),
  });
});

api.get("/stats", async (context) => {
  const stats = await context.env.DB.prepare(
    `SELECT
       COUNT(CASE WHEN status = 'active' THEN 1 END) AS active_count,
       COUNT(CASE WHEN status = 'trashed' THEN 1 END) AS trash_count,
       COALESCE(SUM(CASE WHEN status != 'uploading' THEN byte_size ELSE 0 END), 0) AS stored_bytes,
       COUNT(CASE WHEN status = 'active' AND visibility = 'public' THEN 1 END) AS public_count
     FROM images
     WHERE owner_id = ?`,
  )
    .bind(context.get("ownerId"))
    .first<{
    active_count: number;
    trash_count: number;
    stored_bytes: number;
    public_count: number;
  }>();
  return context.json({
    activeCount: stats?.active_count ?? 0,
    trashCount: stats?.trash_count ?? 0,
    storedBytes: stats?.stored_bytes ?? 0,
    publicCount: stats?.public_count ?? 0,
  });
});

export default api;
