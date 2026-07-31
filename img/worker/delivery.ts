import type { Context } from "hono";
import { verifyShareSignature } from "./crypto";
import { getImageById } from "./db";
import {
  IMAGE_VARIANTS,
  isImageVariant,
  type AppBindings,
  type ImageMimeType,
  type ImageRow,
  type ImageVariant,
  type ShareFormat,
} from "./types";

const MAX_TRANSFORM_BYTES = 20 * 1024 * 1024;
type DeliveryMode = "public" | "signed" | "admin";

function notFound(): Response {
  return new Response("Not found", {
    status: 404,
    headers: {
      "Cache-Control": "private, no-store",
      "X-Content-Type-Options": "nosniff",
    },
  });
}

function cacheControl(mode: DeliveryMode, isVariant: boolean): string {
  if (mode === "public") {
    return "public, max-age=31536000, immutable";
  }
  if (mode === "admin" && isVariant) {
    return "private, max-age=604800, immutable";
  }
  return "private, no-store";
}

function preferredFormat(
  accept: string,
  sourceMime: ImageMimeType,
  requestedFormat: ShareFormat,
): "image/avif" | "image/webp" | "image/jpeg" | "image/gif" {
  if (requestedFormat === "webp") {
    return "image/webp";
  }
  if (accept.includes("image/avif")) {
    return "image/avif";
  }
  if (accept.includes("image/webp")) {
    return "image/webp";
  }
  return sourceMime === "image/gif" ? "image/gif" : "image/jpeg";
}

function requestedFormat(context: Context<AppBindings>): ShareFormat {
  return context.req.query("format") === "webp" ? "webp" : "original";
}

function requestedQuality(context: Context<AppBindings>): number {
  const value = Number(context.req.query("q"));
  return Number.isInteger(value) && value >= 1 && value <= 100 ? value : 85;
}

function baseHeaders(
  image: ImageRow,
  mode: DeliveryMode,
  isVariant = false,
  filename = image.original_name,
): Headers {
  return new Headers({
    "Cache-Control": cacheControl(mode, isVariant),
    "Content-Disposition": `inline; filename*=UTF-8''${encodeURIComponent(filename)}`,
    "X-Content-Type-Options": "nosniff",
    "Cross-Origin-Resource-Policy": "cross-origin",
    "Referrer-Policy": "no-referrer",
  });
}

function webpFilename(originalName: string): string {
  const extensionIndex = originalName.lastIndexOf(".");
  const basename =
    extensionIndex > 0 ? originalName.slice(0, extensionIndex) : originalName;
  return `${basename}.webp`;
}

async function validateSignedRequest(
  context: Context<AppBindings>,
  image: ImageRow,
  variant: ImageVariant,
): Promise<boolean> {
  const expiresAt = Number(context.req.query("exp"));
  const version = Number(context.req.query("v"));
  const signature = context.req.query("sig") ?? "";
  const format = requestedFormat(context);
  const quality = requestedQuality(context);
  if (
    !Number.isSafeInteger(version) ||
    version !== image.share_version ||
    !signature
  ) {
    return false;
  }
  return verifyShareSignature(
    context.env.SHARE_SIGNING_KEY,
    signature,
    image.id,
    variant,
    expiresAt,
    version,
    undefined,
    format,
    quality,
  );
}

async function serveOriginal(
  context: Context<AppBindings>,
  image: ImageRow,
  mode: DeliveryMode,
  transformBypassed = false,
): Promise<Response> {
  const range = context.req.header("Range");
  if (context.req.method === "HEAD") {
    const object = await context.env.IMAGES_BUCKET.head(image.object_key);
    if (!object) {
      return notFound();
    }
    const headers = baseHeaders(image, mode);
    object.writeHttpMetadata(headers);
    headers.set("Content-Type", image.mime_type);
    headers.set("ETag", object.httpEtag);
    headers.set("Accept-Ranges", "bytes");
    headers.set("Content-Length", String(object.size));
    if (transformBypassed) {
      headers.set("X-Nivalis-Transform", "bypassed-size");
    }
    return new Response(null, { status: 200, headers });
  }

  const object = await context.env.IMAGES_BUCKET.get(
    image.object_key,
    range ? { range: context.req.raw.headers } : undefined,
  );
  if (!object) {
    return notFound();
  }

  const headers = baseHeaders(image, mode);
  object.writeHttpMetadata(headers);
  headers.set("Content-Type", image.mime_type);
  headers.set("ETag", object.httpEtag);
  headers.set("Accept-Ranges", "bytes");
  if (transformBypassed) {
    headers.set("X-Nivalis-Transform", "bypassed-size");
  }

  if (context.req.header("If-None-Match") === object.httpEtag) {
    return new Response(null, { status: 304, headers });
  }

  if (range && object.range) {
    const offset =
      "suffix" in object.range
        ? Math.max(0, object.size - object.range.suffix)
        : (object.range.offset ?? 0);
    const length =
      "suffix" in object.range
        ? Math.min(object.range.suffix, object.size)
        : (object.range.length ?? object.size - offset);
    headers.set("Content-Range", `bytes ${offset}-${offset + length - 1}/${object.size}`);
    headers.set("Content-Length", String(length));
  } else {
    headers.set("Content-Length", String(object.size));
  }

  return new Response(object.body, {
    status: range && object.range ? 206 : 200,
    headers,
  });
}

async function serveVariant(
  context: Context<AppBindings>,
  image: ImageRow,
  variant: Exclude<ImageVariant, "original">,
  mode: DeliveryMode,
  shareFormat: ShareFormat,
  webpQuality: number,
): Promise<Response> {
  if (image.byte_size > MAX_TRANSFORM_BYTES) {
    return serveOriginal(context, image, mode, true);
  }

  if (context.req.method === "HEAD") {
    const object = await context.env.IMAGES_BUCKET.head(image.object_key);
    if (!object) {
      return notFound();
    }
    const headers = baseHeaders(
      image,
      mode,
      true,
      shareFormat === "webp" ? webpFilename(image.original_name) : image.original_name,
    );
    headers.set(
      "Content-Type",
      preferredFormat(
        context.req.header("Accept") ?? "",
        image.mime_type,
        shareFormat,
      ),
    );
    return new Response(null, { status: 200, headers });
  }

  const object = await context.env.IMAGES_BUCKET.get(image.object_key);
  if (!object) {
    return notFound();
  }

  const targetWidth = IMAGE_VARIANTS[variant];
  const width =
    image.width && image.width > 0
      ? Math.min(targetWidth, image.width)
      : targetWidth;
  const format = preferredFormat(
    context.req.header("Accept") ?? "",
    image.mime_type,
    shareFormat,
  );
  const transformed = await context.env.IMAGES.input(object.body)
    .transform({ width, fit: "scale-down" })
    .output({
      format,
      quality: shareFormat === "webp" ? webpQuality : 82,
      anim: image.mime_type === "image/gif",
    });
  const result = transformed.response();
  const headers = new Headers(result.headers);
  for (const [name, value] of baseHeaders(
    image,
    mode,
    true,
    shareFormat === "webp" ? webpFilename(image.original_name) : image.original_name,
  )) {
    headers.set(name, value);
  }
  headers.set("Vary", "Accept");
  headers.set(
    "ETag",
    `"${object.etag}-${variant}-${format}-${shareFormat === "webp" ? webpQuality : 82}"`,
  );

  return new Response(result.body, {
    status: result.status,
    headers,
  });
}

export async function deliverImage(
  context: Context<AppBindings>,
  mode: DeliveryMode,
): Promise<Response> {
  const id = context.req.param("id");
  const variantValue = context.req.param("variant");
  if (!id || !variantValue || !isImageVariant(variantValue)) {
    return notFound();
  }

  const image = await getImageById(context.env.DB, id);
  if (!image || image.status !== "active") {
    return notFound();
  }

  if (mode === "admin" && image.owner_id !== context.get("ownerId")) {
    return notFound();
  } else if (mode === "signed") {
    if (
      image.visibility !== "private" ||
      !(await validateSignedRequest(context, image, variantValue))
    ) {
      return notFound();
    }
  } else if (mode === "public" && image.visibility !== "public") {
    return notFound();
  }

  try {
    const shareFormat = requestedFormat(context);
    const webpQuality = requestedQuality(context);
    return variantValue === "original"
      ? await serveOriginal(context, image, mode)
      : await serveVariant(
          context,
          image,
          variantValue,
          mode,
          shareFormat,
          webpQuality,
        );
  } catch (error) {
    console.error(
      JSON.stringify({
        message: "image delivery failed",
        requestId: context.get("requestId"),
        imageId: image.id,
        variant: variantValue,
        error: error instanceof Error ? error.message : String(error),
      }),
    );
    return new Response("Image temporarily unavailable", {
      status: 503,
      headers: {
        "Cache-Control": "private, no-store",
        "Retry-After": "30",
      },
    });
  }
}
