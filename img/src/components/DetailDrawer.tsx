import {
  Check,
  Clock3,
  Copy,
  EyeOff,
  FileImage,
  Gauge,
  Globe2,
  LoaderCircle,
  RotateCcw,
  Save,
  ShieldOff,
  Trash2,
  X,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useI18n } from "../i18n";
import { loadPreferences, type ShareMode } from "../preferences";
import type { ImageRecord, ImageVisibility } from "../types";
import { copyFormats, formatBytes, formatDate, imageDimensions } from "../utils";

interface DetailDrawerProps {
  image: ImageRecord;
  busy: boolean;
  onClose: () => void;
  onSave: (update: {
    title: string;
    altText: string;
    visibility: ImageVisibility;
    tags: string[];
  }) => Promise<void>;
  onShare: (
    expiresIn: 3600 | 86400 | 604800 | 2592000,
    mode: ShareMode,
    webpQuality: number,
  ) => Promise<string>;
  onRevoke: () => Promise<void>;
  onTrash: () => Promise<void>;
  onRestore: () => Promise<void>;
  onDelete: () => Promise<void>;
  onNotify: (message: string) => void;
}

export function DetailDrawer({
  image,
  busy,
  onClose,
  onSave,
  onShare,
  onRevoke,
  onTrash,
  onRestore,
  onDelete,
  onNotify,
}: DetailDrawerProps) {
  const { t, locale } = useI18n();
  const [title, setTitle] = useState(image.title);
  const [altText, setAltText] = useState(image.altText);
  const [visibility, setVisibility] = useState(image.visibility);
  const [tagText, setTagText] = useState(image.tags.join(", "));
  const [expiry, setExpiry] = useState<3600 | 86400 | 604800 | 2592000>(
    86400,
  );
  const preferences = useMemo(loadPreferences, []);
  const [shareMode, setShareMode] = useState<ShareMode>(
    preferences.defaultShareMode,
  );
  const [copied, setCopied] = useState<string | null>(null);
  const isTrash = image.status === "trashed";

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  useEffect(() => {
    setShareMode(preferences.defaultShareMode);
  }, [image.id, preferences.defaultShareMode]);

  const formats = useMemo(
    () =>
      copyFormats(image, shareMode, preferences.webpQuality, {
        originalUrl: t("format.originalUrl"),
        webpUrl: t("format.webpUrl"),
      }),
    [image, preferences.webpQuality, shareMode, t],
  );

  const shareModeControl = (
    <div>
      <label className="form-label">{t("detail.shareMode")}</label>
      <div className="grid grid-cols-2 gap-2">
        <button
          type="button"
          className={`p-3 rounded-xl border text-left flex flex-col gap-1 transition-all ${
            shareMode === "original"
              ? "border-primary bg-primary-fixed/30 text-primary"
              : "border-outline-variant/40 bg-surface-container-low text-on-surface-variant"
          }`}
          onClick={() => setShareMode("original")}
        >
          <span className="flex items-center gap-1.5 font-bold text-xs">
            <FileImage size={15} /> {t("detail.original")}
          </span>
          <span className="text-[11px] opacity-80">
            {t("detail.originalDesc")}
          </span>
        </button>
        <button
          type="button"
          className={`p-3 rounded-xl border text-left flex flex-col gap-1 transition-all ${
            shareMode === "webp"
              ? "border-primary bg-primary-fixed/30 text-primary"
              : "border-outline-variant/40 bg-surface-container-low text-on-surface-variant"
          }`}
          onClick={() => setShareMode("webp")}
        >
          <span className="flex items-center gap-1.5 font-bold text-xs">
            <Gauge size={15} /> {t("detail.webp")}
          </span>
          <span className="text-[11px] opacity-80">
            {t("detail.webpDesc", { quality: preferences.webpQuality })}
          </span>
        </button>
      </div>
    </div>
  );

  const copy = async (label: string, value: string) => {
    await navigator.clipboard.writeText(value);
    setCopied(label);
    onNotify(t("detail.copied", { label }));
    window.setTimeout(() => setCopied(null), 1600);
  };

  return (
    <div
      className="drawer-backdrop"
      role="presentation"
      onMouseDown={onClose}
    >
      <aside
        className="drawer-content p-6 overflow-y-auto space-y-6"
        role="dialog"
        aria-modal="true"
        aria-label={t("detail.imageDetails")}
        onMouseDown={(e) => e.stopPropagation()}
      >
        {/* Header */}
        <div className="flex items-start justify-between">
          <div>
            <span className="text-xs font-semibold text-primary uppercase tracking-wider">
              {t("detail.imageDetails")}
            </span>
            <h2 className="text-xl font-bold text-on-surface truncate max-w-[320px]">
              {image.title || image.originalName}
            </h2>
          </div>
          <button
            type="button"
            className="btn-icon"
            onClick={onClose}
            aria-label={t("common.close")}
          >
            <X size={20} />
          </button>
        </div>

        {/* Preview Image */}
        <div className="rounded-xl overflow-hidden bg-surface-container-high border border-outline-variant/30 max-h-72 flex items-center justify-center">
          <img
            src={image.urls.preview}
            alt={image.altText || image.originalName}
            className="max-h-72 object-contain w-full"
          />
        </div>

        {/* Meta Stats Row */}
        <div className="flex items-center justify-between p-3 bg-surface-container-low rounded-xl text-xs font-mono text-on-surface-variant border border-outline-variant/20">
          <span>{imageDimensions(image)}</span>
          <span>{formatBytes(image.byteSize)}</span>
          <span>{formatDate(image.createdAt, locale)}</span>
        </div>

        {!isTrash && (
          <>
            {/* Information Form */}
            <div className="space-y-4 pt-2">
              <h3 className="font-bold text-sm text-on-surface border-b border-outline-variant/20 pb-2">
                {t("detail.metadata")}
              </h3>

              <div>
                <label className="form-label">{t("detail.title")}</label>
                <input
                  type="text"
                  className="form-input"
                  value={title}
                  maxLength={200}
                  placeholder={t("detail.titlePlaceholder")}
                  onChange={(e) => setTitle(e.target.value)}
                />
              </div>

              <div>
                <label className="form-label">{t("detail.alt")}</label>
                <textarea
                  className="form-input resize-none h-20"
                  value={altText}
                  maxLength={500}
                  placeholder={t("detail.altPlaceholder")}
                  onChange={(e) => setAltText(e.target.value)}
                />
              </div>

              <div>
                <label className="form-label">{t("detail.tags")}</label>
                <input
                  type="text"
                  className="form-input"
                  value={tagText}
                  placeholder={t("detail.tagsPlaceholder")}
                  onChange={(e) => setTagText(e.target.value)}
                />
              </div>

              {/* Visibility Selector */}
              <div>
                <label className="form-label">{t("detail.access")}</label>
                <div className="grid grid-cols-2 gap-2">
                  <button
                    type="button"
                    className={`p-3 rounded-xl border text-left flex flex-col gap-1 transition-all ${
                      visibility === "private"
                        ? "border-primary bg-primary-fixed/30 text-primary"
                        : "border-outline-variant/40 bg-surface-container-low text-on-surface-variant"
                    }`}
                    onClick={() => setVisibility("private")}
                  >
                    <div className="flex items-center gap-1.5 font-bold text-xs">
                      <EyeOff size={15} /> {t("detail.private")}
                    </div>
                    <span className="text-[11px] opacity-80">
                      {t("detail.privateDesc")}
                    </span>
                  </button>

                  <button
                    type="button"
                    className={`p-3 rounded-xl border text-left flex flex-col gap-1 transition-all ${
                      visibility === "public"
                        ? "border-primary bg-primary-fixed/30 text-primary"
                        : "border-outline-variant/40 bg-surface-container-low text-on-surface-variant"
                    }`}
                    onClick={() => setVisibility("public")}
                  >
                    <div className="flex items-center gap-1.5 font-bold text-xs">
                      <Globe2 size={15} /> {t("detail.public")}
                    </div>
                    <span className="text-[11px] opacity-80">
                      {t("detail.publicDesc")}
                    </span>
                  </button>
                </div>
              </div>

              {shareModeControl}

              <button
                type="button"
                className="btn-primary w-full justify-center text-xs py-2.5"
                disabled={busy}
                onClick={() =>
                  void onSave({
                    title,
                    altText,
                    visibility,
                    tags: tagText
                      .split(/[,，]/u)
                      .map((t) => t.trim())
                      .filter(Boolean),
                  })
                }
              >
                {busy ? <LoaderCircle className="animate-spin" size={16} /> : <Save size={16} />}
                {t("detail.save")}
              </button>
            </div>

            {/* Links & Share Section */}
            <div className="space-y-3 pt-2">
              <h3 className="font-bold text-sm text-on-surface border-b border-outline-variant/20 pb-2">
                {t(
                  image.visibility === "public"
                    ? "detail.copyDirect"
                    : "detail.temporary",
                )}
              </h3>

              {image.visibility === "public" ? (
                <div className="space-y-3">
                  {formats.map((fmt) => (
                    <div
                      key={fmt.label}
                      className="p-2.5 bg-surface-container-low rounded-xl border border-outline-variant/20 flex items-center justify-between gap-2 text-xs"
                    >
                      <div className="min-w-0 flex-1">
                        <span className="font-semibold text-on-surface block">{fmt.label}</span>
                        <code className="text-[11px] font-mono text-on-surface-variant truncate block">
                          {fmt.value}
                        </code>
                      </div>
                      <button
                        type="button"
                        className="btn-icon flex-shrink-0"
                        onClick={() => void copy(fmt.label, fmt.value)}
                      >
                        {copied === fmt.label ? <Check size={14} /> : <Copy size={14} />}
                      </button>
                    </div>
                  ))}
                </div>
              ) : (
                <div className="space-y-3">
                  <div>
                    <label className="form-label">{t("detail.expiry")}</label>
                    <select
                      className="form-input text-xs"
                      value={expiry}
                      onChange={(e) =>
                        setExpiry(Number(e.target.value) as any)
                      }
                    >
                      <option value={3600}>{t("detail.hour")}</option>
                      <option value={86400}>{t("detail.day")}</option>
                      <option value={604800}>{t("detail.week")}</option>
                      <option value={2592000}>{t("detail.month")}</option>
                    </select>
                  </div>

                  <button
                    type="button"
                    className="btn-secondary w-full justify-center text-xs py-2"
                    disabled={busy}
                    onClick={() =>
                      void onShare(
                        expiry,
                        shareMode,
                        preferences.webpQuality,
                      ).then((url) =>
                        copy(t("detail.privateLink"), url)
                      )
                    }
                  >
                    <Clock3 size={15} />
                    {t(
                      shareMode === "webp"
                        ? "detail.generateWebp"
                        : "detail.generateOriginal",
                    )}
                  </button>

                  <button
                    type="button"
                    className="text-xs text-error hover:underline w-full text-center"
                    disabled={busy}
                    onClick={() => void onRevoke()}
                  >
                    <ShieldOff size={13} className="inline mr-1" />
                    {t("detail.revoke")}
                  </button>
                </div>
              )}
            </div>
          </>
        )}

        {/* Delete / Recycle Action */}
        <div className="pt-4 border-t border-outline-variant/30">
          {isTrash ? (
            <div className="flex gap-2">
              <button
                type="button"
                className="btn-secondary flex-1 text-xs justify-center"
                disabled={busy}
                onClick={() => void onRestore()}
              >
                <RotateCcw size={15} /> {t("detail.restore")}
              </button>
              <button
                type="button"
                className="btn-secondary flex-1 text-xs justify-center text-error hover:bg-error-container/20"
                disabled={busy}
                onClick={() => void onDelete()}
              >
                <Trash2 size={15} /> {t("detail.deleteForever")}
              </button>
            </div>
          ) : (
            <button
              type="button"
              className="btn-secondary w-full text-xs justify-center text-error hover:bg-error-container/20"
              disabled={busy}
              onClick={() => void onTrash()}
            >
              <Trash2 size={15} /> {t("detail.moveTrash")}
            </button>
          )}
        </div>
      </aside>
    </div>
  );
}
