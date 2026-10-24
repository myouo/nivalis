// @vitest-environment node
import { readFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { URL as NodeURL } from "node:url";
import { Hono } from "hono";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import api from "../../worker/api";
import { deliverImage } from "../../worker/delivery";
import { deleteStoredImage, getStoredImage, parseByteRange } from "../../worker/storage";
import { MAX_IMAGE_BYTES, readBoundedBody, sha256Hex } from "../../shared/limits";
import type { AppBindings, ImageRow } from "../../worker/types";

const bytes = new TextEncoder().encode("original-image-bytes");
let sqlite: DatabaseSync;

function database(): D1Database {
  sqlite = new DatabaseSync(":memory:");
  for (const file of ["0001_initial.sql", "0002_anonymous_owners.sql", "0003_telegram_storage.sql"]) {
    sqlite.exec(readFileSync(new NodeURL(`../../migrations/${file}`, import.meta.url), "utf8"));
  }
  function prepare(sql: string, values: (string | number | null)[] = []): D1PreparedStatement {
    const meta = { duration: 0, size_after: 0, rows_read: 0, rows_written: 0, last_row_id: 0, changed_db: false, changes: 0 };
    const statement = {
      bind: (...args: (string | number | null)[]) => prepare(sql, args),
      first: async <T>(column?: string): Promise<T | null> => {
        const row = sqlite.prepare(sql).get(...values);
        return (column ? row?.[column] ?? null : row ?? null) as T | null;
      },
      run: async () => {
        const result = sqlite.prepare(sql).run(...values);
        return { success: true, results: [], meta: { ...meta, changes: Number(result.changes) } };
      },
      all: async () => ({ success: true, results: sqlite.prepare(sql).all(...values), meta }),
      raw: async () => { throw new Error("This test does not use D1 raw queries"); },
    };
    return statement as D1PreparedStatement;
  }
  return { prepare } as D1Database;
}

function environment() {
  const env = {
    DB: database(),
    IMAGE_STORAGE: "telegram",
    TELEGRAM_BOT_TOKEN: "123456:test-token-that-is-never-production",
    TELEGRAM_CHAT_ID: "-100123456789",
    MAX_UPLOAD_BYTES: String(MAX_IMAGE_BYTES),
    MAX_OWNER_IMAGES: "500",
    MAX_OWNER_STORAGE_BYTES: "1073741824",
    PUBLIC_IMAGE_ORIGIN: "https://images.example.test",
    SHARE_SIGNING_KEY: "a-test-signing-key-with-sufficient-length",
    UPLOAD_USER_RATE_LIMITER: { limit: vi.fn().mockResolvedValue({ success: true }) },
    UPLOAD_IP_RATE_LIMITER: { limit: vi.fn().mockResolvedValue({ success: true }) },
    IMAGES_BUCKET: {
      get: vi.fn().mockResolvedValue(null),
      head: vi.fn().mockResolvedValue(null),
      put: vi.fn().mockResolvedValue({}),
      delete: vi.fn().mockResolvedValue(undefined),
    },
    IMAGES: { info: vi.fn().mockResolvedValue({ width: 10, height: 20 }) },
  };
  return env as typeof env & Env;
}

function application() {
  const app = new Hono<AppBindings>();
  app.use("*", async (context, next) => {
    context.set("ownerId", "owner-one");
    context.set("requestId", "test-request");
    await next();
  });
  app.route("/api", api);
  app.on(["GET", "HEAD"], "/i/:id/:variant/:slug", (context) => deliverImage(context, "public"));
  app.on(["GET", "HEAD"], "/api/media/:id/:variant/:slug", (context) => deliverImage(context, "admin"));
  app.onError(() => new Response("Upload failed", { status: 503 }));
  return app;
}

const executionCtx = { waitUntil: vi.fn(), passThroughOnException: vi.fn(), props: {} };

function mockTelegram(corrupt = false) {
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    // Workers rejects this standard Fetch mode before sending any request.
    if (init?.redirect === "error") throw new TypeError("Workers does not implement redirect:error");
    const url = String(input);
    if (url.endsWith("/sendDocument")) {
      expect(init?.body).toBeInstanceOf(FormData);
      const form = init!.body as FormData;
      expect(form.get("chat_id")).toBe("-100123456789");
      expect(form.get("document")).toBeInstanceOf(Blob);
      return Response.json({ ok: true, result: {
        message_id: 42, date: Math.floor(Date.now() / 1000),
        chat: { id: -100123456789 },
        document: { file_id: "document-id", file_unique_id: "unique-id", file_size: bytes.length },
      } });
    }
    if (url.endsWith("/getFile")) {
      return Response.json({ ok: true, result: { file_path: "documents/original.png", file_size: bytes.length } });
    }
    if (url.includes("/file/bot")) return new Response(corrupt ? "corrupt" : bytes);
    if (url.endsWith("/deleteMessage")) return Response.json({ ok: true, result: true });
    throw new Error("Unexpected Telegram request");
  });
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

async function insertImage(env: Env, overrides: Partial<ImageRow> = {}): Promise<ImageRow> {
  const hash = await sha256Hex(Uint8Array.from(bytes).buffer);
  await env.DB.prepare(`INSERT INTO images
    (id, owner_id, object_key, sha256, original_name, slug, mime_type, byte_size,
     visibility, status, created_at, updated_at, storage_backend)
    VALUES (?, ?, ?, ?, 'original.png', 'original.png', 'image/png', ?, ?, ?, ?, ?, ?)`)
    .bind(overrides.id ?? "existing-id", overrides.owner_id ?? "owner-one",
      overrides.object_key ?? "originals/existing.png", hash,
      overrides.byte_size ?? bytes.length, overrides.visibility ?? "public",
      overrides.status ?? "active", Date.now(), Date.now(), overrides.storage_backend ?? "r2")
    .run();
  return (await env.DB.prepare("SELECT * FROM images WHERE id = ?")
    .bind(overrides.id ?? "existing-id").first<ImageRow>())!;
}

async function insertReference(env: Env, image: ImageRow, backup = 0, age = 0) {
  await env.DB.prepare(`INSERT INTO telegram_objects
    (image_id, file_id, file_unique_id, message_id, chat_id, bot_id, uploaded_at, verified_at, r2_backup)
    VALUES (?, 'document-id', 'unique-id', 42, ?, '123456', ?, ?, ?)`)
    .bind(image.id, env.TELEGRAM_CHAT_ID, Date.now() - age, Date.now(), backup).run();
}

describe("Telegram storage and legacy delivery", () => {
  beforeEach(() => { vi.stubGlobal("caches", undefined); });
  afterEach(() => { sqlite?.close(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

  it("stores and verifies a new upload before making its original URL available", async () => {
    const env = environment();
    const app = application();
    const fetchMock = mockTelegram();
    const initialized = await app.request("/api/uploads", { method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name: "original.png", type: "image/png", size: bytes.length,
        sha256: await sha256Hex(Uint8Array.from(bytes).buffer), visibility: "public" }),
    }, env, executionCtx);
    expect(initialized.status).toBe(201);
    const { uploadId, putUrl } = await initialized.json() as { uploadId: string; putUrl: string };
    const uploaded = await app.request(putUrl, { method: "PUT", headers: { "Content-Type": "image/png" }, body: bytes }, env, executionCtx);
    expect(uploaded.status).toBe(201);
    expect(env.IMAGES_BUCKET.put).not.toHaveBeenCalled();
    const row = await env.DB.prepare("SELECT * FROM images WHERE id = ?").bind(uploadId).first<ImageRow>();
    expect(row?.storage_backend).toBe("telegram");
    expect(await env.DB.prepare("SELECT verified_at FROM telegram_objects WHERE image_id = ?").bind(uploadId).first("verified_at")).toBeTypeOf("number");
    const original = await app.request(`/i/${uploadId}/original/original.png`, {}, env, executionCtx);
    expect(original.status).toBe(200);
    expect(await original.text()).toBe(new TextDecoder().decode(bytes));
    expect(JSON.stringify([...original.headers])).not.toContain(env.TELEGRAM_BOT_TOKEN);
    expect(fetchMock.mock.calls.some(([url]) => String(url).endsWith("/sendDocument"))).toBe(true);
  });

  it("rejects metadata over 20 MB before creating or sending an upload", async () => {
    const env = environment();
    const fetchMock = mockTelegram();
    const response = await application().request("/api/uploads", { method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name: "large.png", type: "image/png", size: MAX_IMAGE_BYTES + 1, sha256: "a".repeat(64) }),
    }, env, executionCtx);
    expect(response.status).toBe(413);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(await env.DB.prepare("SELECT COUNT(*) AS count FROM images").first("count")).toBe(0);
  });

  it("rejects forged content hashes without uploading anything to Telegram", async () => {
    const env = environment();
    const image = await insertImage(env, { status: "uploading" });
    const fetchMock = mockTelegram();
    const response = await application().request(`/api/uploads/${image.id}`, {
      method: "PUT", headers: { "Content-Type": "image/png" }, body: "different-content",
    }, env, executionCtx);
    expect(response.status).toBe(400);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(await env.DB.prepare("SELECT * FROM images WHERE id = ?").bind(image.id).first()).toBeNull();
  });

  it("removes a corrupt Telegram upload instead of activating it", async () => {
    const env = environment();
    const image = await insertImage(env, { status: "uploading" });
    const fetchMock = mockTelegram(true);
    const response = await application().request(`/api/uploads/${image.id}`, {
      method: "PUT", headers: { "Content-Type": "image/png" }, body: bytes,
    }, env, executionCtx);
    expect(response.status).toBe(503);
    expect(await env.DB.prepare("SELECT * FROM images WHERE id = ?").bind(image.id).first()).toBeNull();
    expect(fetchMock.mock.calls.some(([url]) => String(url).endsWith("/deleteMessage"))).toBe(true);
  });

  it("continues serving existing files above 20 MB from R2 with the same URL", async () => {
    const env = environment();
    const image = await insertImage(env, { byte_size: MAX_IMAGE_BYTES + 1 });
    env.IMAGES_BUCKET.get.mockResolvedValue({ body: new Response(bytes).body, size: image.byte_size });
    const fetchMock = mockTelegram();
    const response = await application().request(`/i/${image.id}/w320/original.png`, {}, env, executionCtx);
    expect(response.status).toBe(200);
    expect(response.headers.get("X-Nivalis-Transform")).toBe("bypassed-size");
    expect(await response.text()).toBe(new TextDecoder().decode(bytes));
    expect(fetchMock).not.toHaveBeenCalled();
    expect(env.IMAGES_BUCKET.get).toHaveBeenCalledWith(image.object_key, undefined);
  });

  it("authorizes private images before looking in the Telegram cache", async () => {
    const env = environment();
    const image = await insertImage(env, { visibility: "private", storage_backend: "telegram" });
    await insertReference(env, image);
    const match = vi.fn();
    vi.stubGlobal("caches", { default: { match } });
    const fetchMock = mockTelegram();
    const response = await application().request(`/i/${image.id}/original/original.png`, {}, env, executionCtx);
    expect(response.status).toBe(404);
    expect(match).not.toHaveBeenCalled();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("falls back to a retained R2 copy when Telegram is unavailable", async () => {
    const env = environment();
    const image = await insertImage(env, { storage_backend: "telegram" });
    await insertReference(env, image, 1);
    env.IMAGES_BUCKET.get.mockResolvedValue({ body: new Response(bytes).body, size: bytes.length });
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("unavailable")));
    const stored = await getStoredImage(env, image);
    expect(await new Response(stored!.body).text()).toBe(new TextDecoder().decode(bytes));
  });

  it("keeps cleanup references for old Telegram files while deleting R2 backups", async () => {
    const env = environment();
    const image = await insertImage(env, { storage_backend: "telegram" });
    await insertReference(env, image, 1, 3 * 24 * 60 * 60 * 1000);
    const fetchMock = mockTelegram();
    await deleteStoredImage(env, image);
    expect(env.IMAGES_BUCKET.delete).toHaveBeenCalledWith(image.object_key);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(await env.DB.prepare("SELECT cleanup_error FROM telegram_objects WHERE image_id = ?").bind(image.id).first("cleanup_error"))
      .toBe("message_too_old_for_bot_api");
  });

  it("reserves distinct channel send slots atomically", async () => {
    const env = environment();
    const statement = `INSERT INTO telegram_send_slots (chat_id, next_send_at) VALUES (?, ?)
      ON CONFLICT(chat_id) DO UPDATE SET next_send_at = MAX(next_send_at, ?) + 1100
      WHERE next_send_at <= ? RETURNING next_send_at`;
    const first = await env.DB.prepare(statement).bind("channel", 2100, 1000, 11000).first("next_send_at");
    const second = await env.DB.prepare(statement).bind("channel", 2100, 1000, 11000).first("next_send_at");
    expect(first).toBe(2100);
    expect(second).toBe(3200);
  });
});

describe("bounded uploads and HTTP ranges", () => {
  it("accepts exactly the limit and cancels a body that exceeds it", async () => {
    await expect(readBoundedBody(new Response("abcd").body!, 4)).resolves.toHaveProperty("byteLength", 4);
    const cancel = vi.fn();
    const body = new ReadableStream<Uint8Array>({
      start(controller) { controller.enqueue(new Uint8Array(5)); }, cancel,
    });
    await expect(readBoundedBody(body, 4)).rejects.toBeInstanceOf(RangeError);
    expect(cancel).toHaveBeenCalledOnce();
  });

  it("handles suffix/open ranges and rejects invalid or unsatisfiable ranges", () => {
    expect(parseByteRange("bytes=-3", 10)).toEqual({ offset: 7, length: 3 });
    expect(parseByteRange("bytes=4-", 10)).toEqual({ offset: 4, length: 6 });
    expect(parseByteRange("bytes=0-100", 10)).toEqual({ offset: 0, length: 10 });
    expect(parseByteRange("bytes=10-", 10)).toBeNull();
    expect(parseByteRange("bytes=-0", 10)).toBeNull();
    expect(parseByteRange("bytes=4-2", 10)).toBeNull();
  });
});
