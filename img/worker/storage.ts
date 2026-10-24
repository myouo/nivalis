import {
  downloadTelegramFile,
  TelegramApiError,
  telegramBotId,
  telegramRequest,
  uploadTelegramFile,
  verifyTelegramFile,
  type TelegramConfig,
  type TelegramFileReference,
} from "../shared/telegram";
import type { ImageRow } from "./types";

export interface TelegramObject extends TelegramFileReference {
  image_id: string;
  verified_at: number | null;
  r2_backup: number;
  deleted_at: number | null;
  cleanup_error: string | null;
}

export interface ByteRange {
  offset: number;
  length: number;
}

export interface StoredImage {
  body: ReadableStream<Uint8Array>;
  size: number;
  range?: ByteRange;
}

export function parseByteRange(value: string, size: number): ByteRange | null {
  const match = /^bytes=(\d*)-(\d*)$/u.exec(value);
  if (!match || (!match[1] && !match[2])) return null;
  const start = match[1] ? Number(match[1]) : null;
  const end = match[2] ? Number(match[2]) : null;
  if ((start !== null && !Number.isSafeInteger(start)) ||
      (end !== null && !Number.isSafeInteger(end))) return null;
  if (start === null) {
    if (!end || end < 0) return null;
    const length = Math.min(end, size);
    return { offset: size - length, length };
  }
  if (start >= size || (end !== null && end < start)) return null;
  return { offset: start, length: Math.min(end ?? size - 1, size - 1) - start + 1 };
}

export function telegramConfig(env: Env, object?: TelegramObject): TelegramConfig {
  const config = {
    botToken: env.TELEGRAM_BOT_TOKEN?.trim() ?? "",
    chatId: object?.chat_id ?? env.TELEGRAM_CHAT_ID?.trim() ?? "",
  };
  if (object && telegramBotId(config) !== object.bot_id) {
    throw new Error("The configured Telegram bot does not own this file");
  }
  return config;
}

export function getTelegramObject(env: Env, imageId: string): Promise<TelegramObject | null> {
  return env.DB.prepare("SELECT * FROM telegram_objects WHERE image_id = ?")
    .bind(imageId)
    .first<TelegramObject>();
}

export async function reserveTelegramSend(env: Env): Promise<void> {
  const now = Date.now();
  const slot = await env.DB.prepare(
    `INSERT INTO telegram_send_slots (chat_id, next_send_at) VALUES (?, ?)
     ON CONFLICT(chat_id) DO UPDATE
     SET next_send_at = MAX(telegram_send_slots.next_send_at, ?) + 1100
     WHERE telegram_send_slots.next_send_at <= ?
     RETURNING next_send_at`,
  )
    .bind(env.TELEGRAM_CHAT_ID, now + 1100, now, now + 10_000)
    .first<{ next_send_at: number }>();
  if (!slot) throw new TelegramApiError("upload queue", 429, 5);
  const delay = slot.next_send_at - 1100 - Date.now();
  if (delay > 0) await new Promise((resolve) => setTimeout(resolve, delay));
}

export async function putStoredImage(
  env: Env,
  image: ImageRow,
  bytes: ArrayBuffer,
): Promise<ImageRow["storage_backend"]> {
  if (String(env.IMAGE_STORAGE) === "r2") {
    await env.IMAGES_BUCKET.put(image.object_key, bytes, {
      httpMetadata: { contentType: image.mime_type },
      customMetadata: { originalName: image.original_name, sha256: image.sha256 },
    });
    return "r2";
  }
  if (String(env.IMAGE_STORAGE) !== "telegram") {
    throw new Error("IMAGE_STORAGE must be r2 or telegram");
  }
  const config = telegramConfig(env);
  // Validate credentials before reserving a shared send slot.
  telegramBotId(config);
  await reserveTelegramSend(env);
  let reference: TelegramFileReference;
  try {
    reference = await uploadTelegramFile(config, bytes, image.original_name, image.mime_type);
  } catch (error) {
    if (error instanceof TelegramApiError && error.errorCode === 429) {
      await env.DB.prepare(
        "UPDATE telegram_send_slots SET next_send_at = MAX(next_send_at, ?) WHERE chat_id = ?",
      ).bind(Date.now() + Math.max(error.retryAfter, 1) * 1000, config.chatId).run();
    }
    throw error;
  }
  try {
    // Checkpoint the reference before verification; failed uploads can be cleaned.
    await env.DB.prepare(
      `INSERT INTO telegram_objects
       (image_id, file_id, file_unique_id, message_id, chat_id, bot_id, uploaded_at)
       VALUES (?, ?, ?, ?, ?, ?, ?)`,
    )
      .bind(image.id, reference.file_id, reference.file_unique_id,
        reference.message_id, reference.chat_id, reference.bot_id, reference.uploaded_at)
      .run();
  } catch (error) {
    await telegramRequest(config, "deleteMessage", {
      chat_id: reference.chat_id, message_id: reference.message_id,
    }).catch(() => undefined);
    throw error;
  }
  await verifyTelegramFile(config, reference, image.byte_size, image.sha256);
  await env.DB.prepare("UPDATE telegram_objects SET verified_at = ? WHERE image_id = ?")
    .bind(Date.now(), image.id)
    .run();
  return "telegram";
}

async function getR2Image(env: Env, image: ImageRow, range?: ByteRange): Promise<StoredImage | null> {
  const object = await env.IMAGES_BUCKET.get(
    image.object_key, range ? { range } : undefined,
  );
  if (!object) return null;
  return { body: object.body, size: object.size, range };
}

export async function headStoredImage(env: Env, image: ImageRow): Promise<{ size: number } | null> {
  if (image.storage_backend !== "telegram") {
    return env.IMAGES_BUCKET.head(image.object_key);
  }
  const object = await getTelegramObject(env, image.id);
  return object?.verified_at && !object.deleted_at ? { size: image.byte_size } : null;
}

export async function getStoredImage(
  env: Env,
  image: ImageRow,
  range?: ByteRange,
  executionCtx?: Pick<ExecutionContext, "waitUntil">,
): Promise<StoredImage | null> {
  if (image.storage_backend !== "telegram") return getR2Image(env, image, range);
  const object = await getTelegramObject(env, image.id);
  if (!object?.verified_at || object.deleted_at) return null;
  try {
    // This key contains no credentials. Callers must authorize before this lookup.
    const cacheKey = new Request(new URL(
      `/__storage/telegram/${image.id}/${image.sha256}`, env.PUBLIC_IMAGE_ORIGIN,
    ));
    const cache = executionCtx && typeof caches !== "undefined" ? caches.default : undefined;
    if (!range && cache) {
      const cached = await cache.match(cacheKey).catch(() => undefined);
      if (cached?.body) return { body: cached.body, size: image.byte_size };
    }
    const config = telegramConfig(env, object);
    const requestedRange = range
      ? `bytes=${range.offset}-${range.offset + range.length - 1}` : undefined;
    const upstream = await downloadTelegramFile(config, object.file_id, requestedRange);
    if (upstream.status === 206) {
      const expected = `bytes ${range?.offset}-${range ? range.offset + range.length - 1 : ""}/${image.byte_size}`;
      if (!range || upstream.headers.get("Content-Range") !== expected) {
        await upstream.body!.cancel();
        throw new Error("Telegram returned an unexpected byte range");
      }
      return { body: upstream.body!, size: image.byte_size, range };
    }
    const length = upstream.headers.get("Content-Length");
    if (length && Number(length) !== image.byte_size) {
      await upstream.body!.cancel();
      throw new Error("Telegram returned an unexpected file size");
    }
    const response = new Response(upstream.body, {
      headers: {
        "Content-Type": image.mime_type,
        "Content-Length": String(image.byte_size),
        "Cache-Control": "public, max-age=86400",
      },
    });
    if (!range && cache && executionCtx) {
      executionCtx.waitUntil(cache.put(cacheKey, response.clone()).catch(() => {
        console.warn(JSON.stringify({ message: "Telegram source cache write failed", imageId: image.id }));
      }));
    }
    return { body: response.body!, size: image.byte_size };
  } catch (error) {
    if (object.r2_backup) {
      const backup = await getR2Image(env, image, range);
      if (backup) {
        console.warn(JSON.stringify({ message: "Serving R2 migration backup", imageId: image.id }));
        return backup;
      }
    }
    throw error;
  }
}

export async function deleteStoredImage(env: Env, image: ImageRow): Promise<void> {
  const object = await getTelegramObject(env, image.id);
  if (image.storage_backend !== "telegram" || object?.r2_backup) {
    await env.IMAGES_BUCKET.delete(image.object_key);
  }
  if (!object) return;
  let cleanupError: string | null = null;
  if (Date.now() - object.uploaded_at >= 48 * 60 * 60 * 1000) {
    cleanupError = "message_too_old_for_bot_api";
  } else {
    try {
      await telegramRequest(telegramConfig(env, object), "deleteMessage", {
        chat_id: object.chat_id, message_id: object.message_id,
      });
    } catch {
      cleanupError = "telegram_cleanup_failed";
    }
  }
  await env.DB.prepare(
    `UPDATE telegram_objects SET deleted_at = ?, cleanup_error = ?, r2_backup = 0
     WHERE image_id = ?`,
  ).bind(Date.now(), cleanupError, image.id).run();
  if (cleanupError) {
    console.warn(JSON.stringify({ message: "Telegram copy requires manual cleanup", imageId: image.id, reason: cleanupError }));
  }
}
