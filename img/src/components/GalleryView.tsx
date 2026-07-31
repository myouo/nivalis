import { useMemo, useState } from "react";
import { useI18n } from "../i18n";
import type { ImageRecord, ImageVisibility } from "../types";
import { formatBytes, formatDate } from "../utils";
import { CustomSelect } from "./CustomSelect";

type ImageSort = "recent" | "name" | "size";

interface GalleryViewProps {
  images: ImageRecord[];
  isLoading: boolean;
  isError: boolean;
  error?: Error | null;
  hasNextPage?: boolean;
  isFetchingNextPage?: boolean;
  onFetchNextPage: () => void;
  selectedIds: Set<string>;
  copiedId: string | null;
  busy: boolean;
  isTrash: boolean;
  isSearchActive: boolean;
  visibility: ImageVisibility | "all";
  mimeType: string;
  tag: string;
  onToggleSelect: (id: string, selected: boolean) => void;
  onSelectAll: (select: boolean, ids?: string[]) => void;
  onOpenDetail: (image: ImageRecord) => void;
  onCopyUrl: (image: ImageRecord) => void;
  onTrashImage: (image: ImageRecord) => void;
  onRestoreImage: (image: ImageRecord) => void;
  onBulkDelete: () => void;
  onBulkRestore: () => void;
  onBulkMove: () => void;
  onClearFilters: () => void;
}

export function GalleryView({
  images,
  isLoading,
  isError,
  error,
  hasNextPage,
  isFetchingNextPage,
  onFetchNextPage,
  selectedIds,
  copiedId,
  busy,
  isTrash,
  isSearchActive,
  visibility,
  mimeType,
  tag,
  onToggleSelect,
  onSelectAll,
  onOpenDetail,
  onCopyUrl,
  onTrashImage,
  onRestoreImage,
  onBulkDelete,
  onBulkRestore,
  onBulkMove,
  onClearFilters,
}: GalleryViewProps) {
  const { t, locale } = useI18n();
  const [sortBy, setSortBy] = useState<ImageSort>("recent");
  const sortOptions = [
    { value: "recent", label: t("gallery.recent") },
    { value: "name", label: t("gallery.name") },
    { value: "size", label: t("gallery.size") },
  ] satisfies readonly { value: ImageSort; label: string }[];
  const hasFilters =
    isSearchActive ||
    visibility !== "all" ||
    mimeType !== "all" ||
    Boolean(tag);
  const sortedImages = useMemo(
    () =>
      [...images].sort((a, b) => {
        if (sortBy === "name") {
          return a.originalName.localeCompare(b.originalName);
        }
        if (sortBy === "size") return b.byteSize - a.byteSize;
        return b.createdAt - a.createdAt;
      }),
    [images, sortBy],
  );
  const allSelected =
    images.length > 0 &&
    images.every((image) => selectedIds.has(image.id));

  return (
    <section className="gallery-page">
      <div className="batch-bar">
        <div className="batch-actions">
          <label>
            <input
              type="checkbox"
              name="select-all-images"
              checked={allSelected}
              onChange={(event) =>
                onSelectAll(
                  event.target.checked,
                  images.map((image) => image.id),
                )
              }
            />
            <span>{t("gallery.selectAll")}</span>
          </label>
          <span className="toolbar-divider" />
          {isTrash ? (
            <button
              type="button"
              disabled={busy || selectedIds.size === 0}
              onClick={onBulkRestore}
            >
              <span className="material-symbols-outlined">restore</span>
              {t("gallery.restoreSelected")}
            </button>
          ) : (
            <>
              <button
                type="button"
                className="danger-action"
                disabled={busy}
                onClick={onBulkDelete}
              >
                <span className="material-symbols-outlined">delete</span>
                {t("gallery.deleteSelected")}
              </button>
              <button
                type="button"
                disabled={busy}
                onClick={onBulkMove}
              >
                <span className="material-symbols-outlined">folder_copy</span>
                {t("gallery.moveAlbum")}
              </button>
            </>
          )}
        </div>
        <div className="sort-control">
          <span>{t("gallery.sortBy")}</span>
          <CustomSelect
            value={sortBy}
            options={sortOptions}
            onChange={setSortBy}
            ariaLabel={t("gallery.sortBy")}
            className="sort-select"
          />
        </div>
      </div>

      {isLoading ? (
        <div className="masonry-grid" aria-label={t("gallery.loading")}>
          {Array.from({ length: 6 }).map((_, index) => (
            <div className="image-skeleton" key={index} />
          ))}
        </div>
      ) : isError ? (
        <div className="empty-state">
          <span className="material-symbols-outlined">cloud_off</span>
          <h2>{t("gallery.loadFailed")}</h2>
          <p>{error?.message ?? t("gallery.checkConnection")}</p>
        </div>
      ) : sortedImages.length === 0 ? (
        <div className="empty-state">
          <span className="material-symbols-outlined">
            {isTrash ? "delete" : "photo_library"}
          </span>
          <h2>
            {t(isTrash ? "gallery.trashEmpty" : "gallery.noImages")}
          </h2>
          <p>
            {isTrash
              ? t("gallery.trashEmptyDesc")
              : hasFilters
                ? t("gallery.filterEmptyDesc")
                : t("gallery.emptyDesc")}
          </p>
          {hasFilters ? (
            <button
              type="button"
              className="secondary-button"
              onClick={onClearFilters}
            >
              {t("gallery.clearFilters")}
            </button>
          ) : null}
        </div>
      ) : (
        <div className="masonry-grid">
          {sortedImages.map((image, index) => {
            const selected = selectedIds.has(image.id);
            const aspect =
              image.width && image.height ? image.width / image.height : 4 / 3;
            return (
              <article
                className={`image-card ${selected ? "is-selected" : ""}`}
                key={image.id}
              >
                <div
                  className="image-card-media"
                  style={{ aspectRatio: aspect }}
                  onClick={() => onOpenDetail(image)}
                >
                  <img
                    src={image.urls.thumbnail}
                    alt={image.altText || image.originalName}
                    width={image.width ?? undefined}
                    height={image.height ?? undefined}
                    loading={index < 12 ? "eager" : "lazy"}
                    fetchPriority={index < 4 ? "high" : "auto"}
                    decoding="async"
                    draggable={false}
                  />
                  <label
                    className="image-card-checkbox"
                    onClick={(event) => event.stopPropagation()}
                  >
                    <input
                      type="checkbox"
                      name={`select-image-${image.id}`}
                      checked={selected}
                      onChange={(event) =>
                        onToggleSelect(image.id, event.target.checked)
                      }
                    />
                  </label>
                  <div className="image-card-actions">
                    {!isTrash ? (
                      <button
                        type="button"
                        title={t("gallery.copyLink")}
                        onClick={(event) => {
                          event.stopPropagation();
                          onCopyUrl(image);
                        }}
                      >
                        <span className="material-symbols-outlined">
                          {copiedId === image.id ? "check" : "link"}
                        </span>
                      </button>
                    ) : null}
                    <button
                      type="button"
                      title={t("gallery.edit")}
                      onClick={(event) => {
                        event.stopPropagation();
                        onOpenDetail(image);
                      }}
                    >
                      <span className="material-symbols-outlined">edit</span>
                    </button>
                    {isTrash ? (
                      <button
                        type="button"
                        title={t("gallery.restore")}
                        onClick={(event) => {
                          event.stopPropagation();
                          onRestoreImage(image);
                        }}
                      >
                        <span className="material-symbols-outlined">
                          restore
                        </span>
                      </button>
                    ) : null}
                    <button
                      type="button"
                      className="danger-action"
                      title={t("gallery.delete")}
                      onClick={(event) => {
                        event.stopPropagation();
                        onTrashImage(image);
                      }}
                    >
                      <span className="material-symbols-outlined">delete</span>
                    </button>
                  </div>
                </div>
                <div className="image-card-meta">
                  <h2>{image.title || image.originalName}</h2>
                  <p>
                    <span>
                      {formatDate(image.createdAt, locale)}
                    </span>
                    <span>
                      {formatBytes(image.byteSize)}
                    </span>
                  </p>
                </div>
              </article>
            );
          })}
        </div>
      )}

      {sortedImages.length > 0 ? (
        <div className="load-more-area">
          {hasNextPage || isFetchingNextPage ? (
            <button
              type="button"
              disabled={isFetchingNextPage}
              onClick={onFetchNextPage}
            >
              {t(
                isFetchingNextPage
                  ? "gallery.loadingMore"
                  : "gallery.loadMore",
              )}
              <span className="material-symbols-outlined">expand_more</span>
            </button>
          ) : null}
          <p>
            {t("gallery.showing", {
              count: sortedImages.length,
              total: sortedImages.length,
            })}
          </p>
        </div>
      ) : null}
    </section>
  );
}
