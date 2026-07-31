import { NavLink } from "react-router-dom";
import type { ActiveView, Stats } from "../types";
import { useI18n, type MessageKey } from "../i18n";
import { pathForView } from "../routes";

interface SidebarProps {
  view: ActiveView;
  stats?: Stats;
  onCloseMobile: () => void;
  isOpenMobile: boolean;
}

const navigation: Array<{
  view: ActiveView;
  icon: string;
  label: MessageKey;
}> = [
  { view: "upload", icon: "cloud_upload", label: "nav.upload" },
  { view: "gallery", icon: "image", label: "nav.gallery" },
  { view: "albums", icon: "photo_album", label: "nav.albums" },
  { view: "trash", icon: "delete", label: "nav.trash" },
  { view: "settings", icon: "settings", label: "nav.settings" },
];

export function Sidebar({
  view,
  stats,
  onCloseMobile,
  isOpenMobile,
}: SidebarProps) {
  const { t } = useI18n();
  const imageCount = stats?.activeCount ?? 0;

  return (
    <>
      {isOpenMobile ? (
        <button
          className="mobile-nav-backdrop"
          type="button"
          aria-label={t("nav.close")}
          onClick={onCloseMobile}
        />
      ) : null}

      <aside
        className={`sidebar sidebar-${view} ${
          isOpenMobile ? "is-open" : ""
        }`}
      >
        <div className="sidebar-header">
          <div className="sidebar-brand-row">
            <div>
              <NavLink
                to={pathForView("gallery")}
                className="sidebar-title"
                onClick={onCloseMobile}
              >
                nivalis
              </NavLink>
              <p className="sidebar-subtitle">
                {t("library.imageCount", {
                  count: imageCount,
                  unit: t(
                    imageCount === 1 ? "library.image" : "library.images",
                  ),
                })}
              </p>
            </div>
            <button
              type="button"
              className="mobile-close"
              aria-label={t("nav.close")}
              onClick={onCloseMobile}
            >
              <span className="material-symbols-outlined">close</span>
            </button>
          </div>

        </div>

        <nav className="sidebar-nav" aria-label={t("nav.main")}>
          {navigation.map((item) => (
            <NavLink
              key={item.view}
              to={pathForView(item.view)}
              end
              className={({ isActive }) =>
                `sidebar-nav-item ${isActive ? "is-active" : ""}`
              }
              onClick={onCloseMobile}
            >
              <span className="material-symbols-outlined">{item.icon}</span>
              <span>{t(item.label)}</span>
            </NavLink>
          ))}
        </nav>

      </aside>
    </>
  );
}
