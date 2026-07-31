import { IMAGE_VARIANTS, type ImageRow } from "./types";
import { imagePublicUrl } from "./db";

function isLocalRequest(request: Request): boolean {
  const { hostname } = new URL(request.url);
  return hostname === "localhost" || hostname === "127.0.0.1";
}

export async function purgeImageCache(
  request: Request,
  env: Env,
  image: Pick<ImageRow, "id" | "slug">,
): Promise<void> {
  if (isLocalRequest(request)) {
    return;
  }

  if (!env.CF_API_TOKEN || env.ZONE_ID.startsWith("replace-")) {
    throw new Error(
      "Cache purge is not configured. Set CF_API_TOKEN and ZONE_ID first.",
    );
  }

  const files = Object.keys(IMAGE_VARIANTS).map((variant) =>
    imagePublicUrl(env, image, variant as keyof typeof IMAGE_VARIANTS),
  );
  const response = await fetch(
    `https://api.cloudflare.com/client/v4/zones/${env.ZONE_ID}/purge_cache`,
    {
      method: "POST",
      headers: {
        Authorization: `Bearer ${env.CF_API_TOKEN}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ files }),
    },
  );

  if (!response.ok) {
    throw new Error(`Cloudflare cache purge returned ${response.status}`);
  }
}
