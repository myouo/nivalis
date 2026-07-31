import { useEffect, useMemo, useRef, useState } from "react";
import { useI18n } from "../i18n";
import type { Album, ImageRecord } from "../types";

interface AlbumsViewProps {
  images: ImageRecord[];
  query: string;
  createRequest: number;
  onSelectAlbum: (albumTitle: string) => void;
  onNotify: (message: string) => void;
}

function loadAlbums(): Album[] {
  try {
    const saved = localStorage.getItem("nivalis-albums");
    return saved ? (JSON.parse(saved) as Album[]) : [];
  } catch {
    return [];
  }
}

export function AlbumsView({
  images,
  query,
  createRequest,
  onSelectAlbum,
  onNotify,
}: AlbumsViewProps) {
  const { t } = useI18n();
  const [albums, setAlbums] = useState<Album[]>(loadAlbums);
  const [viewMode, setViewMode] = useState<"grid" | "list">("grid");
  const [showCreateModal, setShowCreateModal] = useState(false);
  const [newTitle, setNewTitle] = useState("");
  const [newDescription, setNewDescription] = useState("");
  const lastCreateRequest = useRef(createRequest);

  useEffect(() => {
    localStorage.setItem("nivalis-albums", JSON.stringify(albums));
  }, [albums]);

  useEffect(() => {
    if (createRequest !== lastCreateRequest.current) {
      lastCreateRequest.current = createRequest;
      setShowCreateModal(true);
    }
  }, [createRequest]);

  const visibleAlbums = useMemo(() => {
    const term = query.trim().toLocaleLowerCase();
    if (!term) return albums;
    return albums.filter((album) =>
      `${album.title} ${album.description ?? ""}`
        .toLocaleLowerCase()
        .includes(term),
    );
  }, [albums, query]);

  const createAlbum = (event: React.FormEvent) => {
    event.preventDefault();
    const title = newTitle.trim();
    if (!title) return;
    setAlbums((current) => [
      {
        id: crypto.randomUUID(),
        title,
        description: newDescription.trim() || undefined,
        coverUrl: images[0]?.urls.thumbnail,
        imageCount: 0,
        createdAt: Date.now(),
      },
      ...current,
    ]);
    setNewTitle("");
    setNewDescription("");
    setShowCreateModal(false);
    onNotify(t("albums.created", { name: title }));
  };

  const renameAlbum = (album: Album) => {
    const nextTitle = window.prompt(t("albums.renamePrompt"), album.title)?.trim();
    if (!nextTitle) return;
    setAlbums((current) =>
      current.map((item) =>
        item.id === album.id ? { ...item, title: nextTitle } : item,
      ),
    );
    onNotify(t("albums.renamed"));
  };

  const removeAlbum = (album: Album) => {
    if (!window.confirm(t("albums.deletePrompt", { name: album.title }))) {
      return;
    }
    setAlbums((current) => current.filter((item) => item.id !== album.id));
    onNotify(t("albums.deleted"));
  };

  return (
    <section className="albums-page">
      <nav className="breadcrumbs" aria-label="Breadcrumb">
        <button type="button" onClick={() => onSelectAlbum("")}>
          <span className="material-symbols-outlined">home</span>
          <span>{t("albums.home")}</span>
        </button>
        <span className="material-symbols-outlined">chevron_right</span>
        <strong>{t("nav.albums")}</strong>
      </nav>

      <div className="page-heading">
        <div>
          <h1>{t("albums.title")}</h1>
          <p>{t("albums.description")}</p>
        </div>
        <div className="view-toggle" aria-label={t("nav.albums")}>
          <button
            type="button"
            className={viewMode === "grid" ? "is-active" : ""}
            onClick={() => setViewMode("grid")}
            aria-label={t("albums.grid")}
          >
            <span className="material-symbols-outlined">grid_view</span>
          </button>
          <button
            type="button"
            className={viewMode === "list" ? "is-active" : ""}
            onClick={() => setViewMode("list")}
            aria-label={t("albums.list")}
          >
            <span className="material-symbols-outlined">list</span>
          </button>
        </div>
      </div>

      {viewMode === "grid" ? (
        <div className="album-grid">
          {visibleAlbums.map((album) => (
            <article
              key={album.id}
              className="album-card"
              onClick={() => onSelectAlbum(album.title)}
            >
              <div className="album-card-cover">
                {album.coverUrl ? (
                  <img src={album.coverUrl} alt="" />
                ) : (
                  <span className="album-cover-placeholder material-symbols-outlined">
                    photo_album
                  </span>
                )}
                <div className="album-actions">
                  <button
                    type="button"
                    title={t("albums.rename")}
                    onClick={(event) => {
                      event.stopPropagation();
                      renameAlbum(album);
                    }}
                  >
                    <span className="material-symbols-outlined">edit</span>
                  </button>
                  <button
                    type="button"
                    title={t("albums.share")}
                    onClick={(event) => {
                      event.stopPropagation();
                      void navigator.clipboard.writeText(
                        `${location.origin}/albums/${album.id}`,
                      );
                      onNotify(t("albums.linkCopied"));
                    }}
                  >
                    <span className="material-symbols-outlined">share</span>
                  </button>
                  <button
                    type="button"
                    title={t("albums.delete")}
                    onClick={(event) => {
                      event.stopPropagation();
                      removeAlbum(album);
                    }}
                  >
                    <span className="material-symbols-outlined">delete</span>
                  </button>
                </div>
              </div>
              <div className="album-card-body">
                <div>
                  <h2>{album.title}</h2>
                  <p>{t("albums.imageCount", { count: album.imageCount })}</p>
                </div>
                <span className="material-symbols-outlined">folder_open</span>
              </div>
            </article>
          ))}

          <button
            type="button"
            className="create-album-card"
            onClick={() => setShowCreateModal(true)}
          >
            <span className="create-folder-icon">
              <span className="material-symbols-outlined">
                create_new_folder
              </span>
            </span>
            <strong>{t("albums.create")}</strong>
          </button>
        </div>
      ) : (
        <div className="album-list">
          {visibleAlbums.map((album) => (
            <button
              type="button"
              key={album.id}
              onClick={() => onSelectAlbum(album.title)}
            >
              {album.coverUrl ? (
                <img src={album.coverUrl} alt="" />
              ) : (
                <span className="album-cover-placeholder material-symbols-outlined">
                  photo_album
                </span>
              )}
              <span>
                <strong>{album.title}</strong>
                <small>
                  {album.description ??
                    t("albums.imageCount", { count: album.imageCount })}
                </small>
              </span>
              <small>{t("albums.itemCount", { count: album.imageCount })}</small>
              <span className="material-symbols-outlined">chevron_right</span>
            </button>
          ))}
        </div>
      )}

      {showCreateModal ? (
        <div className="modal-backdrop" onMouseDown={() => setShowCreateModal(false)}>
          <form
            className="dialog-card"
            onSubmit={createAlbum}
            onMouseDown={(event) => event.stopPropagation()}
          >
            <div className="dialog-heading">
              <div>
                <span className="eyebrow">{t("albums.newCollection")}</span>
                <h2>{t("albums.create")}</h2>
              </div>
              <button
                type="button"
                className="icon-button"
                onClick={() => setShowCreateModal(false)}
                aria-label={t("common.close")}
              >
                <span className="material-symbols-outlined">close</span>
              </button>
            </div>
            <label className="form-field">
              <span>{t("albums.albumName")}</span>
              <input
                name="album-name"
                autoFocus
                required
                value={newTitle}
                onChange={(event) => setNewTitle(event.target.value)}
                placeholder={t("albums.albumPlaceholder")}
              />
            </label>
            <label className="form-field">
              <span>{t("albums.descriptionLabel")}</span>
              <textarea
                name="album-description"
                value={newDescription}
                onChange={(event) => setNewDescription(event.target.value)}
                placeholder={t("albums.descriptionPlaceholder")}
              />
            </label>
            <div className="dialog-actions">
              <button
                type="button"
                className="secondary-button"
                onClick={() => setShowCreateModal(false)}
              >
                {t("albums.cancel")}
              </button>
              <button type="submit" className="primary-button">
                {t("albums.create")}
              </button>
            </div>
          </form>
        </div>
      ) : null}
    </section>
  );
}
