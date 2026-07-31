import type { ImageRecord } from "./types";
import type { ShareMode } from "./preferences";

export function absolutePublicUrl(value: string): string {
  if (typeof window === "undefined") return value;
  try {
    return new URL(value, window.location.origin).toString();
  } catch {
    return value;
  }
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value >= 10 ? value.toFixed(1) : value.toFixed(2)} ${units[index]}`;
}

export function formatDate(timestamp: number, locale = "zh-CN"): string {
  return new Intl.DateTimeFormat(locale, {
    year: "numeric",
    month: "short",
    day: "numeric",
  }).format(timestamp);
}

export function shareUrl(
  image: ImageRecord,
  mode: ShareMode,
  webpQuality = 85,
): string {
  const originalUrl = absolutePublicUrl(image.urls.original);
  if (mode === "original") return originalUrl;
  try {
    const url = new URL(originalUrl);
    if (!url.pathname.includes("/original/")) return originalUrl;
    url.pathname = url.pathname.replace("/original/", "/w1920/");
    url.searchParams.set("format", "webp");
    url.searchParams.set("q", String(webpQuality));
    return url.toString();
  } catch {
    return originalUrl;
  }
}

export function copyFormats(
  image: ImageRecord,
  mode: ShareMode = "original",
  webpQuality = 85,
  labels: { originalUrl: string; webpUrl: string } = {
    originalUrl: "Original URL",
    webpUrl: "WebP URL",
  },
): {
  label: string;
  value: string;
}[] {
  const selectedUrl = shareUrl(image, mode, webpQuality);
  return [
    {
      label: mode === "original" ? labels.originalUrl : labels.webpUrl,
      value: selectedUrl,
    },
    {
      label: "Markdown",
      value: `![${image.altText || image.title || image.originalName}](${selectedUrl})`,
    },
    {
      label: "HTML",
      value: `<img src="${selectedUrl}" alt="${image.altText || image.title || image.originalName}" />`,
    },
    { label: "BBCode", value: `[img]${selectedUrl}[/img]` },
  ];
}

export function imageDimensions(image: ImageRecord): string {
  return image.width && image.height
    ? `${image.width} × ${image.height}`
    : "尺寸未解析";
}
