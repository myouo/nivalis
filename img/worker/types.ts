export const IMAGE_MIME_TYPES = [
  "image/jpeg",
  "image/png",
  "image/webp",
  "image/avif",
  "image/gif",
] as const;

export const IMAGE_VARIANTS = {
  original: null,
  w320: 320,
  w640: 640,
  w1280: 1280,
  w1920: 1920,
} as const;

export type ImageMimeType = (typeof IMAGE_MIME_TYPES)[number];
export type ImageVariant = keyof typeof IMAGE_VARIANTS;
export type ShareFormat = "original" | "webp";
export type ImageVisibility = "public" | "private";
export type ImageStatus = "uploading" | "active" | "trashed";

export interface ImageRow {
  id: string;
  owner_id: string;
  object_key: string;
  sha256: string;
  original_name: string;
  slug: string;
  mime_type: ImageMimeType;
  byte_size: number;
  width: number | null;
  height: number | null;
  title: string;
  alt_text: string;
  visibility: ImageVisibility;
  status: ImageStatus;
  share_version: number;
  created_at: number;
  updated_at: number;
  deleted_at: number | null;
}

export interface TagRow {
  id: string;
  name: string;
  normalized_name: string;
  created_at: number;
}

export interface ImageResponse {
  id: string;
  originalName: string;
  slug: string;
  mimeType: ImageMimeType;
  byteSize: number;
  width: number | null;
  height: number | null;
  title: string;
  altText: string;
  visibility: ImageVisibility;
  status: ImageStatus;
  tags: string[];
  createdAt: number;
  updatedAt: number;
  deletedAt: number | null;
  urls: {
    original: string;
    thumbnail: string;
    preview: string;
  };
}

export interface ApiErrorBody {
  code: string;
  message: string;
  requestId: string;
}

export interface RequestVariables {
  requestId: string;
  ownerId: string;
}

export type AppBindings = {
  Bindings: Env;
  Variables: RequestVariables;
};

export function isImageVariant(value: string): value is ImageVariant {
  return Object.hasOwn(IMAGE_VARIANTS, value);
}

export function isImageMimeType(value: string): value is ImageMimeType {
  return IMAGE_MIME_TYPES.some((type) => type === value);
}
