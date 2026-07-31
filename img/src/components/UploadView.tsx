import {
  useCallback,
  useEffect,
  useMemo,
  useState,
} from "react";
import { useI18n } from "../i18n";
import type { UploadQueueItem } from "../types";
import { absolutePublicUrl, formatBytes } from "../utils";

interface UploadViewProps {
  items: UploadQueueItem[];
  onFiles: (files: File[]) => void;
  onBrowse: () => void;
  onClear: () => void;
  onNotify: (message: string) => void;
}

interface UploadListItem {
  id: string;
  name: string;
  progress: number;
  status: UploadQueueItem["status"];
  image?: string;
  resultUrl?: string;
  size?: number;
}

export function UploadView({
  items,
  onFiles,
  onBrowse,
  onClear,
  onNotify,
}: UploadViewProps) {
  const { t } = useI18n();
  const [dragging, setDragging] = useState(false);
  const [selectedId, setSelectedId] = useState<string | null>(null);

  const acceptFiles = useCallback(
    (files: FileList | File[] | null) => {
      if (!files) return;
      const accepted = Array.from(files).filter((file) =>
        file.type.startsWith("image/"),
      );
      if (accepted.length > 0) onFiles(accepted);
    },
    [onFiles],
  );

  const recentItems = useMemo<UploadListItem[]>(() => {
    return items.slice(0, 6).map((item) => ({
      id: item.id,
      name: item.file.name,
      progress: item.progress,
      status: item.status,
      size: item.file.size,
      image: item.previewUrl ?? item.resultPreviewUrl ?? item.resultUrl,
      resultUrl: item.resultUrl,
    }));
  }, [items]);

  useEffect(() => {
    if (selectedId && recentItems.some((item) => item.id === selectedId)) {
      return;
    }
    const preferred =
      recentItems.find(
        (item) => item.status === "done" && Boolean(item.resultUrl),
      ) ?? recentItems[0];
    setSelectedId(preferred?.id ?? null);
  }, [recentItems, selectedId]);

  const selectedItem =
    recentItems.find((item) => item.id === selectedId) ?? null;
  const directUrl =
    selectedItem?.status === "done" && selectedItem.resultUrl
      ? absolutePublicUrl(selectedItem.resultUrl)
      : undefined;
  const linkFormats = [
    { label: t("upload.directUrl"), value: directUrl ?? "" },
    {
      label: "Markdown",
      value: directUrl ? `![${selectedItem?.name ?? "Nivalis image"}](${directUrl})` : "",
    },
    {
      label: "HTML",
      value: directUrl
        ? `<img src="${directUrl}" alt="${selectedItem?.name ?? "Nivalis image"}">`
        : "",
    },
    { label: "BBCode", value: directUrl ? `[img]${directUrl}[/img]` : "" },
  ];

  const copy = async (value: string, message = t("upload.linkCopied")) => {
    if (!value) {
      onNotify(t("upload.stillUploading"));
      return;
    }
    await navigator.clipboard.writeText(value);
    onNotify(message);
  };

  const selectUpload = (id: string) => setSelectedId(id);

  return (
    <section className="upload-page">
      <button
        type="button"
        className={`upload-dropzone ${dragging ? "is-dragging" : ""}`}
        onClick={onBrowse}
        onDragEnter={(event) => {
          event.preventDefault();
          setDragging(true);
        }}
        onDragOver={(event) => event.preventDefault()}
        onDragLeave={(event) => {
          if (event.currentTarget === event.target) setDragging(false);
        }}
        onDrop={(event) => {
          event.preventDefault();
          setDragging(false);
          acceptFiles(event.dataTransfer.files);
        }}
      >
        <span className="upload-icon-circle">
          <span className="material-symbols-outlined">cloud_upload</span>
        </span>
        <strong>{t("upload.dropTitle")}</strong>
        <small>{t("upload.dropHint")}</small>
      </button>

      <div className="upload-workspace">
        <div className="recent-uploads">
          <div className="section-title-row">
            <div>
              <h2>{t("upload.uploaded")}</h2>
              <small>{t("upload.selectHint")}</small>
            </div>
            <div>
              <span>
                {t("upload.uploading", {
                  count: items.filter(
                    (item) =>
                      item.status !== "done" && item.status !== "error",
                  ).length,
                })}
              </span>
              {items.some(
                (item) => item.status === "done" || item.status === "error",
              ) ? (
                <button type="button" onClick={onClear}>
                  {t("upload.clearFinished")}
                </button>
              ) : null}
            </div>
          </div>

          <div className="upload-list">
            {recentItems.length === 0 ? (
              <div className="upload-list-empty">
                <span className="material-symbols-outlined">image</span>
                <p>{t("upload.empty")}</p>
              </div>
            ) : null}
            {recentItems.map((item) => {
              const done = item.status === "done";
              const error = item.status === "error";
              const selected = selectedId === item.id;
              return (
                <article
                  key={item.id}
                  className={`upload-row ${done ? "is-complete" : ""} ${
                    error ? "is-error" : ""
                  } ${selected ? "is-selected" : ""}`}
                  role="button"
                  tabIndex={0}
                  aria-pressed={selected}
                  onClick={() => selectUpload(item.id)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" || event.key === " ") {
                      event.preventDefault();
                      selectUpload(item.id);
                    }
                  }}
                >
                  <span className="upload-select-indicator" aria-hidden="true">
                    <span className="material-symbols-outlined">
                      {selected ? "radio_button_checked" : "radio_button_unchecked"}
                    </span>
                  </span>
                  <div className="upload-thumbnail">
                    {item.image ? (
                      <>
                        <img
                          src={item.image}
                          alt={item.name}
                          onError={(event) => {
                            event.currentTarget.hidden = true;
                            const fallback = event.currentTarget
                              .nextElementSibling;
                            if (fallback instanceof HTMLElement) {
                              fallback.hidden = false;
                            }
                          }}
                        />
                        <span
                          className="material-symbols-outlined"
                          hidden
                        >
                          broken_image
                        </span>
                      </>
                    ) : (
                      <span className="material-symbols-outlined">image</span>
                    )}
                  </div>
                  <div className="upload-row-body">
                    <div className="upload-row-meta">
                      <strong>{item.name}</strong>
                      <span className={error ? "is-error" : ""}>
                        {error ? t("upload.error") : done ? (
                          <>
                            <span className="material-symbols-outlined">
                              check_circle
                            </span>
                            {t("upload.complete")}
                          </>
                        ) : (
                          `${item.progress}%`
                        )}
                      </span>
                    </div>
                    <div className="progress-track">
                      <div
                        className={error ? "is-error" : ""}
                        style={{ width: `${Math.max(item.progress, 3)}%` }}
                      />
                    </div>
                    {item.size ? <small>{formatBytes(item.size)}</small> : null}
                  </div>
                  <button
                    type="button"
                    className="icon-button"
                    disabled={!done || !item.resultUrl}
                    onClick={(event) => {
                      event.stopPropagation();
                      if (item.resultUrl) void copy(item.resultUrl);
                    }}
                    aria-label={t(
                      done ? "upload.copyLink" : "upload.inProgress",
                    )}
                  >
                    <span className="material-symbols-outlined">
                      {done ? "content_copy" : "schedule"}
                    </span>
                  </button>
                </article>
              );
            })}
          </div>
        </div>

        <div className="grab-links">
          <div className="grab-links-heading">
            <h2>{t("upload.grabLinks")}</h2>
            <small>{selectedItem?.name ?? t("upload.selectImage")}</small>
          </div>
          <div className="link-panel" aria-live="polite">
            {!directUrl ? (
              <p className="link-panel-status">
                {selectedItem
                  ? t("upload.linksPending")
                  : t("upload.linksEmpty")}
              </p>
            ) : null}
            {linkFormats.map((format) => (
              <label key={format.label} className="link-field">
                <span>{format.label}</span>
                <span className="link-input-wrap">
                  <input
                    name={`link-${format.label.toLowerCase().replaceAll(" ", "-")}`}
                    readOnly
                    value={format.value}
                    placeholder={t("upload.waiting")}
                  />
                  <button
                    type="button"
                    disabled={!format.value}
                    onClick={() => void copy(format.value)}
                    aria-label={`Copy ${format.label}`}
                  >
                    <span className="material-symbols-outlined">
                      content_copy
                    </span>
                  </button>
                </span>
              </label>
            ))}
          </div>
        </div>
      </div>
    </section>
  );
}
