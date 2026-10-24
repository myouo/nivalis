import { useCallback, useEffect, useRef, useState } from "react";
import {
  initializeUpload,
  RequestError,
  sha256,
  transferUpload,
} from "../api";
import type { UploadQueueItem } from "../types";
import { useI18n } from "../i18n";
import { MAX_IMAGE_BYTES } from "../../shared/limits";

const ACCEPTED_TYPES = new Set([
  "image/jpeg",
  "image/png",
  "image/webp",
  "image/avif",
  "image/gif",
]);
const CONCURRENCY = 3;

interface UseUploadQueueOptions {
  onUploaded: () => void;
}

export function useUploadQueue({ onUploaded }: UseUploadQueueOptions) {
  const { t } = useI18n();
  const [items, setItems] = useState<UploadQueueItem[]>([]);
  const itemsRef = useRef<UploadQueueItem[]>([]);
  const activeCount = useRef(0);
  const pending = useRef<UploadQueueItem[]>([]);

  useEffect(() => {
    itemsRef.current = items;
  }, [items]);

  useEffect(
    () => () => {
      for (const item of itemsRef.current) {
        if (item.previewUrl) URL.revokeObjectURL(item.previewUrl);
      }
    },
    [],
  );

  const patchItem = useCallback(
    (id: string, update: Partial<UploadQueueItem>) => {
      setItems((current) =>
        current.map((item) => (item.id === id ? { ...item, ...update } : item)),
      );
    },
    [],
  );

  const runNext = useCallback(() => {
    while (activeCount.current < CONCURRENCY && pending.current.length > 0) {
      const item = pending.current.shift();
      if (!item) break;
      activeCount.current += 1;

      void (async () => {
        try {
          patchItem(item.id, {
            status: "hashing",
            message: t("upload.checking"),
          });
          const hash = await sha256(item.file);
          const initialized = await initializeUpload(item.file, hash);
          if (initialized.duplicate) {
            patchItem(item.id, {
              status: "done",
              progress: 100,
              message: t(
                initialized.restored
                  ? "upload.restored"
                  : "upload.duplicate",
              ),
              resultUrl: initialized.image.urls.original,
              resultPreviewUrl: initialized.image.urls.preview,
            });
          } else {
            patchItem(item.id, {
              status: "uploading",
              message: t("upload.uploadingStatus"),
            });
            const image = await transferUpload(initialized.putUrl, item.file, (progress) =>
              patchItem(item.id, { progress }),
            );
            patchItem(item.id, {
              status: "done",
              progress: 100,
              message: t("upload.done"),
              resultUrl: image.urls.original,
              resultPreviewUrl: image.urls.preview,
            });
          }
          onUploaded();
        } catch (error) {
          patchItem(item.id, {
            status: "error",
            message:
              error instanceof RequestError || error instanceof Error
                ? error.message
                : t("upload.failed"),
          });
        } finally {
          activeCount.current -= 1;
          runNext();
        }
      })();
    }
  }, [onUploaded, patchItem, t]);

  const addFiles = useCallback(
    (incoming: File[]) => {
      const nextItems = incoming.map<UploadQueueItem>((file) => {
        const error = !ACCEPTED_TYPES.has(file.type)
          ? t("upload.unsupported")
          : file.size > MAX_IMAGE_BYTES
            ? t("upload.tooLarge")
            : undefined;
        return {
          id: crypto.randomUUID(),
          file,
          previewUrl: URL.createObjectURL(file),
          progress: 0,
          status: error ? "error" : "queued",
          message: error,
        };
      });
      setItems((current) => {
        const merged = [...nextItems, ...current];
        for (const item of merged.slice(30)) {
          if (item.previewUrl) URL.revokeObjectURL(item.previewUrl);
        }
        return merged.slice(0, 30);
      });
      pending.current.push(
        ...nextItems.filter((item) => item.status === "queued"),
      );
      queueMicrotask(runNext);
    },
    [runNext, t],
  );

  const clearFinished = useCallback(() => {
    setItems((current) => {
      const retained = current.filter(
        (item) => item.status !== "done" && item.status !== "error",
      );
      const retainedIds = new Set(retained.map((item) => item.id));
      for (const item of current) {
        if (!retainedIds.has(item.id) && item.previewUrl) {
          URL.revokeObjectURL(item.previewUrl);
        }
      }
      return retained;
    });
  }, []);

  return { items, addFiles, clearFinished };
}
