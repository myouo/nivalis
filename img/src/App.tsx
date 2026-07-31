import {
  useInfiniteQuery,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import {
  useCallback,
  useDeferredValue,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useLocation, useNavigate } from "react-router-dom";
import {
  createShareLink,
  getStats,
  getTags,
  listImages,
  permanentlyDeleteImage,
  restoreImage,
  revokeShareLinks,
  trashImage,
  updateImage,
  updateTags,
} from "./api";
import { AlbumsView } from "./components/AlbumsView";
import { DetailDrawer } from "./components/DetailDrawer";
import { GalleryView } from "./components/GalleryView";
import { SettingsView } from "./components/SettingsView";
import { Sidebar } from "./components/Sidebar";
import { UploadView } from "./components/UploadView";
import { useUploadQueue } from "./hooks/useUploadQueue";
import { useI18n, type MessageKey } from "./i18n";
import { pathForView, viewFromPathname } from "./routes";
import type { ActiveView, ImageRecord, ImageVisibility } from "./types";
import { absolutePublicUrl } from "./utils";

const TOPBAR_CONFIG: Record<
  ActiveView,
  { placeholder: MessageKey; action: MessageKey; icon: string }
> = {
  upload: {
    placeholder: "top.searchUpload",
    action: "top.quickUpload",
    icon: "add",
  },
  gallery: {
    placeholder: "top.searchGallery",
    action: "top.quickUpload",
    icon: "add",
  },
  albums: {
    placeholder: "top.searchAlbums",
    action: "top.createAlbum",
    icon: "add",
  },
  settings: {
    placeholder: "top.searchSettings",
    action: "top.quickUpload",
    icon: "add",
  },
  trash: {
    placeholder: "top.searchTrash",
    action: "top.backGallery",
    icon: "arrow_back",
  },
};

const VIEW_LABELS: Record<ActiveView, MessageKey> = {
  upload: "nav.upload",
  gallery: "nav.gallery",
  albums: "nav.albums",
  trash: "nav.trash",
  settings: "nav.settings",
};

export default function App() {
  const { t } = useI18n();
  const location = useLocation();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const matchedView = viewFromPathname(location.pathname);
  const view = matchedView ?? "upload";
  const [query, setQuery] = useState("");
  const deferredQuery = useDeferredValue(query);
  const [visibility, setVisibility] = useState<ImageVisibility | "all">("all");
  const [mimeType, setMimeType] = useState("all");
  const [tag, setTag] = useState("");
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [detail, setDetail] = useState<ImageRecord | null>(null);
  const [busy, setBusy] = useState(false);
  const [toast, setToast] = useState<string | null>(null);
  const [copiedId, setCopiedId] = useState<string | null>(null);
  const [mobileNav, setMobileNav] = useState(false);
  const [albumCreateRequest, setAlbumCreateRequest] = useState(0);
  const fileInputRef = useRef<HTMLInputElement>(null);

  const filters = useMemo(
    () => ({
      status: view === "trash" ? ("trashed" as const) : ("active" as const),
      query:
        view === "gallery" || view === "trash"
          ? deferredQuery.trim() || undefined
          : undefined,
      visibility,
      type: mimeType,
      tag: tag || undefined,
    }),
    [deferredQuery, mimeType, tag, view, visibility],
  );

  const imagesQuery = useInfiniteQuery({
    queryKey: ["images", filters],
    initialPageParam: "",
    queryFn: ({ pageParam }) =>
      listImages({ ...filters, cursor: pageParam || undefined }),
    getNextPageParam: (lastPage) => lastPage.nextCursor ?? undefined,
  });
  const statsQuery = useQuery({ queryKey: ["stats"], queryFn: getStats });
  const tagsQuery = useQuery({ queryKey: ["tags"], queryFn: getTags });

  const images = useMemo(
    () => imagesQuery.data?.pages.flatMap((page) => page.items) ?? [],
    [imagesQuery.data],
  );
  const tagsList = useMemo(
    () => tagsQuery.data?.items.map((item) => item.name) ?? [],
    [tagsQuery.data],
  );

  const refreshLibrary = useCallback(() => {
    void Promise.all([
      queryClient.invalidateQueries({ queryKey: ["images"] }),
      queryClient.invalidateQueries({ queryKey: ["stats"] }),
      queryClient.invalidateQueries({ queryKey: ["tags"] }),
    ]);
  }, [queryClient]);
  const uploadQueue = useUploadQueue({ onUploaded: refreshLibrary });

  const goToView = useCallback(
    (nextView: ActiveView, replace = false) => {
      navigate(pathForView(nextView), { replace });
    },
    [navigate],
  );

  const openFilePicker = useCallback(() => {
    fileInputRef.current?.click();
  }, []);

  const notify = useCallback((message: string) => {
    setToast(message);
    window.setTimeout(() => setToast(null), 2800);
  }, []);

  useEffect(() => {
    const canonicalPath = pathForView(view);
    if (matchedView === null || location.pathname !== canonicalPath) {
      navigate(canonicalPath, { replace: true });
    }
  }, [location.pathname, matchedView, navigate, view]);

  useEffect(() => {
    document.title = `Nivalis · ${t(VIEW_LABELS[view])}`;
  }, [t, view]);

  useEffect(() => {
    const onPaste = (event: ClipboardEvent) => {
      const target = event.target;
      if (
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement
      ) {
        return;
      }
      const files = Array.from(event.clipboardData?.files ?? []).filter((file) =>
        file.type.startsWith("image/"),
      );
      if (files.length === 0) return;
      uploadQueue.addFiles(files);
      goToView("upload");
      notify(t("app.clipboardAdded", { count: files.length }));
    };
    document.addEventListener("paste", onPaste);
    return () => document.removeEventListener("paste", onPaste);
  }, [goToView, notify, t, uploadQueue.addFiles]);

  useEffect(() => {
    setSelectedIds(new Set());
    setDetail(null);
    if (view !== "gallery" && view !== "trash") {
      setVisibility("all");
      setMimeType("all");
      setTag("");
    }
  }, [view]);

  const runAction = useCallback(
    async (action: () => Promise<void>, success: string) => {
      setBusy(true);
      try {
        await action();
        notify(success);
        refreshLibrary();
      } catch (error) {
        notify(error instanceof Error ? error.message : t("app.actionFailed"));
      } finally {
        setBusy(false);
      }
    },
    [notify, refreshLibrary, t],
  );

  const toggleSelected = (id: string, selected: boolean) => {
    setSelectedIds((current) => {
      const next = new Set(current);
      if (selected) next.add(id);
      else next.delete(id);
      return next;
    });
  };

  const toggleSelectAll = (select: boolean, ids?: string[]) => {
    setSelectedIds(select ? new Set(ids ?? images.map((image) => image.id)) : new Set());
  };

  const copyPublicUrl = async (image: ImageRecord) => {
    await navigator.clipboard.writeText(absolutePublicUrl(image.urls.original));
    setCopiedId(image.id);
    notify(t("upload.linkCopied"));
    window.setTimeout(() => setCopiedId(null), 1600);
  };

  const handleTrash = async (image: ImageRecord) => {
    if (image.status === "trashed") {
      if (
        !window.confirm(
          t("app.deletePrompt", {
            name: image.title || image.originalName,
          }),
        )
      ) {
        return;
      }
      await runAction(async () => {
        await permanentlyDeleteImage(image.id);
        setDetail(null);
      }, t("app.deleted"));
      return;
    }
    await runAction(async () => {
      await trashImage(image.id);
      setDetail(null);
    }, t("app.trashed"));
  };

  const handleRestore = async (image: ImageRecord) => {
    await runAction(async () => {
      await restoreImage(image.id);
      setDetail(null);
    }, t("app.restored"));
  };

  const selectedImages = () =>
    images.filter((image) => selectedIds.has(image.id));

  const handleBulkDelete = async () => {
    const selected = selectedImages();
    if (selected.length === 0) return;
    const permanently = view === "trash";
    if (
      permanently &&
      !window.confirm(t("app.bulkDeletePrompt", { count: selected.length }))
    ) {
      return;
    }
    await runAction(async () => {
      await Promise.all(
        selected.map((image) =>
          permanently
            ? permanentlyDeleteImage(image.id)
            : trashImage(image.id),
        ),
      );
      setSelectedIds(new Set());
    }, t(permanently ? "app.bulkDeleted" : "app.bulkTrashed"));
  };

  const handleBulkRestore = async () => {
    const selected = selectedImages();
    if (selected.length === 0) return;
    await runAction(async () => {
      await Promise.all(selected.map((image) => restoreImage(image.id)));
      setSelectedIds(new Set());
    }, t("app.bulkRestored"));
  };

  const handleBulkMove = async () => {
    const selected = selectedImages();
    if (selected.length === 0) return;
    const album =
      window.prompt(
        t("app.movePrompt", { count: selected.length }),
        tagsList[0] ?? t("app.favorites"),
      )?.trim() ?? "";
    if (!album) return;
    await runAction(async () => {
      await Promise.all(
        selected.map((image) =>
          updateTags(image.id, Array.from(new Set([...image.tags, album]))),
        ),
      );
      setSelectedIds(new Set());
    }, t("app.moved", { name: album }));
  };

  const clearFilters = () => {
    setQuery("");
    setVisibility("all");
    setMimeType("all");
    setTag("");
  };

  const runPrimaryAction = () => {
    if (view === "albums") {
      setAlbumCreateRequest((current) => current + 1);
      return;
    }
    if (view === "trash") {
      goToView("gallery");
      return;
    }
    openFilePicker();
  };

  const topbar = TOPBAR_CONFIG[view];

  return (
    <div className="app-shell">
      <Sidebar
        view={view}
        stats={statsQuery.data}
        onCloseMobile={() => setMobileNav(false)}
        isOpenMobile={mobileNav}
      />

      <header className={`topbar topbar-${view}`}>
        <div className="topbar-left">
          <button
            type="button"
            className="mobile-menu-button"
            onClick={() => setMobileNav(true)}
            aria-label={t("nav.open")}
          >
            <span className="material-symbols-outlined">menu</span>
          </button>
          <label className="topbar-search">
            <span className="material-symbols-outlined">search</span>
            <input
              name="nivalis-search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder={t(topbar.placeholder)}
            />
            {query ? (
              <button
                type="button"
                onClick={() => setQuery("")}
                aria-label={t("top.clearSearch")}
              >
                <span className="material-symbols-outlined">close</span>
              </button>
            ) : null}
          </label>
        </div>

        <div className="topbar-actions">
          <button
            type="button"
            className="topbar-primary"
            onClick={runPrimaryAction}
          >
            <span className="material-symbols-outlined">{topbar.icon}</span>
            {t(topbar.action)}
          </button>
          <button
            type="button"
            className="topbar-icon"
            aria-label={t("top.notifications")}
            onClick={() => notify(t("top.caughtUp"))}
          >
            <span className="material-symbols-outlined">notifications</span>
            {view === "albums" ? <i /> : null}
          </button>
        </div>
      </header>

      <main className="main-canvas">
        <div className="content-container">
          {view === "upload" ? (
            <UploadView
              items={uploadQueue.items}
              onFiles={uploadQueue.addFiles}
              onBrowse={openFilePicker}
              onClear={uploadQueue.clearFinished}
              onNotify={notify}
            />
          ) : view === "albums" ? (
            <AlbumsView
              images={images}
              query={query}
              createRequest={albumCreateRequest}
              onSelectAlbum={(album) => {
                if (album) setTag(album);
                setQuery("");
                goToView("gallery");
              }}
              onNotify={notify}
            />
          ) : view === "settings" ? (
            <SettingsView
              stats={statsQuery.data}
              onNotify={notify}
            />
          ) : (
            <GalleryView
              images={images}
              isLoading={imagesQuery.isLoading}
              isError={imagesQuery.isError}
              error={imagesQuery.error}
              hasNextPage={imagesQuery.hasNextPage}
              isFetchingNextPage={imagesQuery.isFetchingNextPage}
              onFetchNextPage={() => void imagesQuery.fetchNextPage()}
              selectedIds={selectedIds}
              copiedId={copiedId}
              busy={busy}
              isTrash={view === "trash"}
              isSearchActive={Boolean(deferredQuery.trim())}
              visibility={visibility}
              mimeType={mimeType}
              tag={tag}
              onToggleSelect={toggleSelected}
              onSelectAll={toggleSelectAll}
              onOpenDetail={setDetail}
              onCopyUrl={(image) => void copyPublicUrl(image)}
              onTrashImage={(image) => void handleTrash(image)}
              onRestoreImage={(image) => void handleRestore(image)}
              onBulkDelete={() => void handleBulkDelete()}
              onBulkRestore={() => void handleBulkRestore()}
              onBulkMove={() => void handleBulkMove()}
              onClearFilters={clearFilters}
            />
          )}
        </div>

        <footer className={`site-footer site-footer-${view}`}>
          <div>
            <strong>© 2026 nivalis</strong>
            <span className="footer-note">{t("footer.personal")}</span>
          </div>
          {view === "albums" ? (
            <span className="edge-status">
              <span className="material-symbols-outlined">bolt</span>
              {t("footer.edge")}
            </span>
          ) : null}
        </footer>
      </main>

      {detail ? (
        <DetailDrawer
          image={detail}
          busy={busy}
          onClose={() => setDetail(null)}
          onNotify={notify}
          onSave={async (update) => {
            await runAction(async () => {
              const metadata = await updateImage(detail.id, {
                title: update.title,
                altText: update.altText,
                visibility: update.visibility,
              });
              const tagged = await updateTags(detail.id, update.tags);
              setDetail(tagged.image ?? metadata.image);
            }, t("app.detailsSaved"));
          }}
          onShare={async (expiresIn, mode, webpQuality) => {
            const result = await createShareLink(
              detail.id,
              expiresIn,
              mode,
              webpQuality,
            );
            return result.url;
          }}
          onRevoke={async () => {
            await runAction(async () => {
              await revokeShareLinks(detail.id);
            }, t("app.linksRevoked"));
          }}
          onTrash={() => handleTrash(detail)}
          onRestore={() => handleRestore(detail)}
          onDelete={() => handleTrash(detail)}
        />
      ) : null}

      <input
        ref={fileInputRef}
        type="file"
        hidden
        multiple
        accept="image/jpeg,image/png,image/webp,image/avif,image/gif"
        aria-hidden="true"
        tabIndex={-1}
        onChange={(event) => {
          const files = Array.from(event.currentTarget.files ?? []);
          event.currentTarget.value = "";
          if (files.length === 0) return;
          uploadQueue.addFiles(files);
          goToView("upload");
        }}
      />

      {toast ? (
        <div className="toast-floating" role="status">
          <span className="material-symbols-outlined">check_circle</span>
          {toast}
        </div>
      ) : null}
    </div>
  );
}
