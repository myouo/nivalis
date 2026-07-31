import { Hono } from "hono";
import api from "./api";
import {
  createAnonymousSession,
  requireAnonymousIdentity,
  requireTrustedOrigin,
} from "./auth";
import { deliverImage } from "./delivery";
import { cleanupExpiredImages } from "./lifecycle";
import type { AppBindings } from "./types";

const app = new Hono<AppBindings>();

app.use("*", async (context, next) => {
  const requestId = context.req.header("CF-Ray") ?? crypto.randomUUID();
  context.set("requestId", requestId);
  await next();
  context.header("X-Request-Id", requestId);
});

app.use("/api/*", async (context, next) => {
  await next();
  context.header("Cache-Control", "private, no-store");
  context.header("Vary", "Cookie");
  context.header("X-Content-Type-Options", "nosniff");
});
app.use("/api/*", requireTrustedOrigin);
app.post("/api/session", createAnonymousSession);
app.use("/api/*", requireAnonymousIdentity);

app.on(["GET", "HEAD"], "/api/media/:id/:variant/:slug", (context) =>
  deliverImage(context, "admin"),
);
app.route("/api", api);
app.on(["GET", "HEAD"], "/i/:id/:variant/:slug", (context) =>
  deliverImage(context, "public"),
);
app.on(["GET", "HEAD"], "/s/:id/:variant/:slug", (context) =>
  deliverImage(context, "signed"),
);

app.notFound(async (context) => {
  const requestUrl = new URL(context.req.url);
  const adminUrl = new URL(context.env.ADMIN_ORIGIN);
  const isLocal =
    requestUrl.hostname === "localhost" ||
    requestUrl.hostname === "127.0.0.1";

  if (
    requestUrl.pathname.startsWith("/api/") ||
    requestUrl.pathname.startsWith("/i/") ||
    requestUrl.pathname.startsWith("/s/") ||
    (!isLocal && requestUrl.hostname !== adminUrl.hostname)
  ) {
    return new Response("Not found", {
      status: 404,
      headers: { "Cache-Control": "private, no-store" },
    });
  }

  const response = await context.env.ASSETS.fetch(context.req.raw);
  const headers = new Headers(response.headers);
  headers.set("X-Content-Type-Options", "nosniff");
  headers.set("Referrer-Policy", "strict-origin-when-cross-origin");
  headers.set("Permissions-Policy", "camera=(), microphone=(), geolocation=()");
  headers.set(
    "Content-Security-Policy",
    `default-src 'self'; img-src 'self' https: data: blob:; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' https://fonts.gstatic.com; script-src 'self'${isLocal ? " 'unsafe-inline'" : ""}; connect-src 'self' ws: wss:; object-src 'none'; base-uri 'self'; frame-ancestors 'none'`,
  );
  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers,
  });
});

app.onError((error, context) => {
  const requestId = context.get("requestId") || crypto.randomUUID();
  console.error(
    JSON.stringify({
      message: "request failed",
      requestId,
      method: context.req.method,
      path: context.req.path,
      error: error.message,
    }),
  );
  return context.json(
    {
      code: "INTERNAL_ERROR",
      message: "服务暂时不可用，请稍后重试。",
      requestId,
    },
    500,
  );
});

export default {
  fetch(request, env, ctx) {
    return app.fetch(request, env, ctx);
  },
  scheduled(_controller, env, ctx) {
    ctx.waitUntil(cleanupExpiredImages(env));
  },
} satisfies ExportedHandler<Env>;
