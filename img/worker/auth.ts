import { getCookie, setCookie } from "hono/cookie";
import type { Context, MiddlewareHandler } from "hono";
import type { AppBindings } from "./types";

const SESSION_COOKIE = "nivalis_session";
const SESSION_MAX_AGE_SECONDS = 365 * 24 * 60 * 60;
const SESSION_TOKEN_PATTERN = /^[a-f0-9]{64}$/u;

function isLocalRequest(request: Request): boolean {
  const { hostname } = new URL(request.url);
  return hostname === "localhost" || hostname === "127.0.0.1";
}

function clientAddress(request: Request): string {
  return (
    request.headers.get("CF-Connecting-IP") ??
    request.headers.get("X-Forwarded-For")?.split(",")[0]?.trim() ??
    "unknown"
  );
}

function randomSessionToken(): string {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join(
    "",
  );
}

async function ownerIdForToken(token: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(token),
  );
  return Array.from(new Uint8Array(digest), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

export async function createAnonymousSession(
  context: Context<AppBindings>,
): Promise<Response> {
  const currentToken = getCookie(context, SESSION_COOKIE);
  if (currentToken && SESSION_TOKEN_PATTERN.test(currentToken)) {
    return context.json({ ok: true });
  }

  const { success } = await context.env.SESSION_RATE_LIMITER.limit({
    key: clientAddress(context.req.raw),
  });
  if (!success) {
    return context.json(
      {
        code: "SESSION_RATE_LIMITED",
        message: "创建匿名会话过于频繁，请稍后再试。",
        requestId: context.get("requestId"),
      },
      429,
    );
  }

  setCookie(context, SESSION_COOKIE, randomSessionToken(), {
    httpOnly: true,
    secure: !isLocalRequest(context.req.raw),
    sameSite: "Strict",
    path: "/",
    maxAge: SESSION_MAX_AGE_SECONDS,
  });
  return context.json({ ok: true }, 201);
}

export const requireAnonymousIdentity: MiddlewareHandler<AppBindings> = async (
  context,
  next,
) => {
  const token = getCookie(context, SESSION_COOKIE);
  if (!token || !SESSION_TOKEN_PATTERN.test(token)) {
    return context.json(
      {
        code: "ANONYMOUS_SESSION_REQUIRED",
        message: "匿名会话不存在，请刷新页面后重试。",
        requestId: context.get("requestId"),
      },
      401,
    );
  }

  context.set("ownerId", await ownerIdForToken(token));
  await next();
};

export const requireTrustedOrigin: MiddlewareHandler<AppBindings> = async (
  context,
  next,
) => {
  if (
    context.req.method === "GET" ||
    context.req.method === "HEAD" ||
    isLocalRequest(context.req.raw)
  ) {
    await next();
    return;
  }

  const origin = context.req.header("Origin");
  if (origin !== context.env.ADMIN_ORIGIN) {
    return context.json(
      {
        code: "INVALID_ORIGIN",
        message: "请求来源不受信任。",
        requestId: context.get("requestId"),
      },
      403,
    );
  }
  await next();
};
