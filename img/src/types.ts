export type ImageVisibility = "public" | "private";
export type ImageStatus = "active" | "trashed";
export type ImageVariant = "original" | "w320" | "w640" | "w1280" | "w1920";

export type ActiveView = "upload" | "gallery" | "albums" | "settings" | "trash";

export interface Album {
  id: string;
  title: string;
  coverUrl?: string;
  imageCount: number;
  createdAt: number;
  description?: string;
}

export interface ImageRecord {
  id: string;
  originalName: string;
  slug: string;
  mimeType: string;
  byteSize: number;
  width: number | null;
  height: number | null;
  title: string;
  altText: string;
  visibility: ImageVisibility;
  status: ImageStatus;
  albumId?: string;
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

export interface ImageListResponse {
  items: ImageRecord[];
  nextCursor: string | null;
}

export interface Stats {
  activeCount: number;
  trashCount: number;
  storedBytes: number;
  publicCount: number;
}

export interface UploadQueueItem {
  id: string;
  file: File;
  previewUrl?: string;
  progress: number;
  status: "queued" | "hashing" | "uploading" | "done" | "error";
  message?: string;
  resultUrl?: string;
  resultPreviewUrl?: string;
}

export interface ApiError {
  code: string;
  message: string;
  requestId?: string;
}
