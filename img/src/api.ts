import type {
  ApiError,
  ImageListResponse,
  ImageRecord,
  ImageVisibility,
  Stats,
} from "./types";
import type { ShareMode } from "./preferences";

export class RequestError extends Error {
  readonly code: string;
  readonly requestId?: string;

  constructor(error: ApiError) {
    super(error.message);
    this.name = "RequestError";
    this.code = error.code;
    this.requestId = error.requestId;
  }
}

let anonymousSessionPromise: Promise<void> | null = null;

async function ensureAnonymousSession(): Promise<void> {
  if (!anonymousSessionPromise) {
    anonymousSessionPromise = fetch("/api/session", {
      method: "POST",
      headers: { Accept: "application/json" },
    }).then(async (response) => {
      if (response.ok) return;
      const payload = (await response.json().catch(() => null)) as
        | Partial<ApiError>
        | null;
      throw new RequestError({
        code: payload?.code ?? "SESSION_FAILED",
        message: payload?.message ?? `无法创建匿名会话（${response.status}）。`,
        requestId: payload?.requestId,
      });
    });
  }

  try {
    await anonymousSessionPromise;
  } catch (error) {
    anonymousSessionPromise = null;
    throw error;
  }
}

async function apiRequest<T>(
  path: string,
  init?: RequestInit,
): Promise<T> {
  await ensureAnonymousSession();
  const response = await fetch(path, {
    ...init,
    headers: {
      Accept: "application/json",
      ...(init?.body ? { "Content-Type": "application/json" } : {}),
      ...init?.headers,
    },
  });

  if (!response.ok) {
    const fallback: ApiError = {
      code: "REQUEST_FAILED",
      message: `请求失败（${response.status}）`,
    };
    const payload: unknown = await response.json().catch(() => fallback);
    const error =
      payload &&
      typeof payload === "object" &&
      "code" in payload &&
      "message" in payload &&
      typeof Reflect.get(payload, "code") === "string" &&
      typeof Reflect.get(payload, "message") === "string"
        ? {
            code: Reflect.get(payload, "code") as string,
            message: Reflect.get(payload, "message") as string,
            requestId:
              typeof Reflect.get(payload, "requestId") === "string"
                ? (Reflect.get(payload, "requestId") as string)
                : undefined,
          }
        : fallback;
    throw new RequestError(error);
  }
  return (await response.json()) as T;
}

export interface ImageFilters {
  status: "active" | "trashed";
  query?: string;
  visibility?: ImageVisibility | "all";
  type?: string;
  tag?: string;
  cursor?: string;
}

export function listImages(filters: ImageFilters): Promise<ImageListResponse> {
  const search = new URLSearchParams({
    status: filters.status,
    limit: "40",
    _v: "2",
  });
  if (filters.query) search.set("q", filters.query);
  if (filters.visibility && filters.visibility !== "all") {
    search.set("visibility", filters.visibility);
  }
  if (filters.type && filters.type !== "all") search.set("type", filters.type);
  if (filters.tag) search.set("tag", filters.tag);
  if (filters.cursor) search.set("cursor", filters.cursor);
  return apiRequest(`/api/images?${search.toString()}`);
}

export function getStats(): Promise<Stats> {
  return apiRequest("/api/stats?_v=2");
}

export function getTags(): Promise<{ items: { id: string; name: string }[] }> {
  return apiRequest("/api/tags?_v=2");
}

export function updateImage(
  id: string,
  update: {
    title?: string;
    altText?: string;
    visibility?: ImageVisibility;
  },
): Promise<{ image: ImageRecord }> {
  return apiRequest(`/api/images/${id}`, {
    method: "PATCH",
    body: JSON.stringify(update),
  });
}

export function updateTags(
  id: string,
  tags: string[],
): Promise<{ image: ImageRecord }> {
  return apiRequest(`/api/images/${id}/tags`, {
    method: "PUT",
    body: JSON.stringify({ tags }),
  });
}

export function trashImage(id: string): Promise<{ ok: true }> {
  return apiRequest(`/api/images/${id}/trash`, { method: "POST" });
}

export function restoreImage(id: string): Promise<{ image: ImageRecord }> {
  return apiRequest(`/api/images/${id}/restore`, { method: "POST" });
}

export function permanentlyDeleteImage(id: string): Promise<{ ok: true }> {
  return apiRequest(`/api/images/${id}`, { method: "DELETE" });
}

export function createShareLink(
  id: string,
  expiresIn: 3600 | 86400 | 604800 | 2592000,
  format: ShareMode = "original",
  quality = 85,
): Promise<{ url: string; expiresAt: number }> {
  const variant = format === "webp" ? "w1920" : "original";
  return apiRequest<{ url: string; expiresAt: number }>(
    `/api/images/${id}/share-links`,
    {
      method: "POST",
      body: JSON.stringify({ expiresIn, variant, format, quality }),
    },
  ).then((result) => ({
    ...result,
    url: new URL(result.url, window.location.origin).toString(),
  }));
}

export function revokeShareLinks(id: string): Promise<{ ok: true }> {
  return apiRequest(`/api/images/${id}/revoke-links`, { method: "POST" });
}

export async function sha256(file: File): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", await file.arrayBuffer());
  return Array.from(new Uint8Array(digest))
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

export async function initializeUpload(
  file: File,
  hash: string,
): Promise<
  | { duplicate: false; uploadId: string; putUrl: string }
  | { duplicate: true; image: ImageRecord; restored?: boolean }
> {
  return apiRequest("/api/uploads", {
    method: "POST",
    body: JSON.stringify({
      name: file.name,
      type: file.type,
      size: file.size,
      sha256: hash,
      visibility: "public",
    }),
  });
}

export function transferUpload(
  putUrl: string,
  file: File,
  onProgress: (progress: number) => void,
): Promise<ImageRecord> {
  return new Promise((resolve, reject) => {
    const request = new XMLHttpRequest();
    request.open("PUT", putUrl);
    request.setRequestHeader("Content-Type", file.type);
    request.setRequestHeader("Accept", "application/json");
    request.upload.addEventListener("progress", (event) => {
      if (event.lengthComputable) {
        onProgress(Math.round((event.loaded / event.total) * 100));
      }
    });
    request.addEventListener("load", () => {
      try {
        const payload = JSON.parse(request.responseText) as
          | { image: ImageRecord }
          | ApiError;
        if (request.status >= 200 && request.status < 300 && "image" in payload) {
          resolve(payload.image);
          return;
        }
        const error =
          "message" in payload
            ? payload
            : { code: "UPLOAD_FAILED", message: "上传失败，请重试。" };
        reject(new RequestError(error));
      } catch {
        reject(
          new RequestError({
            code: "UPLOAD_FAILED",
            message: `上传失败（${request.status}）。`,
          }),
        );
      }
    });
    request.addEventListener("error", () =>
      reject(
        new RequestError({
          code: "NETWORK_ERROR",
          message: "网络中断，图片未上传。",
        }),
      ),
    );
    request.send(file);
  });
}
