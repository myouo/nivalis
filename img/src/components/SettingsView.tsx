import { useState } from "react";
import { useI18n, type LocalePreference } from "../i18n";
import {
  loadPreferences,
  savePreferences,
  type ShareMode,
} from "../preferences";
import type { Stats } from "../types";
import { formatBytes } from "../utils";
import { CustomSelect } from "./CustomSelect";

interface SettingsViewProps {
  stats?: Stats;
  onNotify: (message: string) => void;
}

export function SettingsView({ stats, onNotify }: SettingsViewProps) {
  const { localePreference, setLocalePreference, t } = useI18n();
  const initial = loadPreferences();
  const [quality, setQuality] = useState(initial.webpQuality);
  const [language, setLanguage] =
    useState<LocalePreference>(localePreference);
  const [defaultShareMode, setDefaultShareMode] = useState<ShareMode>(
    initial.defaultShareMode,
  );
  const languageOptions = [
    { value: "auto", label: t("settings.languageAuto") },
    { value: "zh-CN", label: "简体中文" },
    { value: "zh-TW", label: "繁體中文" },
    { value: "en", label: "English" },
    { value: "ja", label: "日本語" },
  ] satisfies readonly { value: LocalePreference; label: string }[];

  const save = () => {
    savePreferences({
      webpQuality: quality,
      defaultShareMode,
    });
    setLocalePreference(language);
    onNotify(t("settings.saved"));
  };

  return (
    <section className="settings-page">
      <header className="settings-heading">
        <h1>{t("settings.title")}</h1>
        <p>{t("settings.description")}</p>
      </header>

      <div className="settings-section">
        <article className="settings-card">
          <h2>{t("settings.interface")}</h2>
          <div className="form-field">
            <span>{t("settings.language")}</span>
            <CustomSelect
              value={language}
              options={languageOptions}
              onChange={setLanguage}
              ariaLabel={t("settings.language")}
              className="settings-language-select"
            />
            <small className="field-help">{t("settings.languageHint")}</small>
          </div>
        </article>

        <article className="settings-card">
          <h2>{t("settings.uploadSharing")}</h2>
          <div className="form-field">
            <span>{t("settings.defaultShare")}</span>
            <div
              className="share-default-options"
              role="group"
              aria-label={t("settings.defaultShare")}
            >
              <button
                type="button"
                className={
                  defaultShareMode === "original" ? "is-active" : ""
                }
                onClick={() => setDefaultShareMode("original")}
              >
                <span className="material-symbols-outlined">image</span>
                <span>
                  <strong>{t("settings.original")}</strong>
                  <small>{t("settings.originalDesc")}</small>
                </span>
              </button>
              <button
                type="button"
                className={defaultShareMode === "webp" ? "is-active" : ""}
                onClick={() => setDefaultShareMode("webp")}
              >
                <span className="material-symbols-outlined">compress</span>
                <span>
                  <strong>{t("settings.webp")}</strong>
                  <small>{t("settings.webpDesc")}</small>
                </span>
              </button>
            </div>
          </div>

          <label className="form-field">
            <span>{t("settings.quality")}</span>
            <input
              type="range"
              name="compression-quality"
              min="1"
              max="100"
              value={quality}
              onChange={(event) => setQuality(Number(event.target.value))}
            />
            <span className="range-labels">
              <small>{t("settings.smaller")}</small>
              <strong>{quality}%</strong>
              <small>{t("settings.sharper")}</small>
            </span>
          </label>
        </article>

        <article className="settings-card usage-card">
          <div>
            <h2>{t("settings.overview")}</h2>
            <p>{t("settings.overviewDesc")}</p>
          </div>
          <div className="usage-summary">
            <div className="usage-number">
              <strong>{formatBytes(stats?.storedBytes ?? 0)}</strong>
              <span>
                {t("settings.storedAcross", {
                  count: stats?.activeCount ?? 0,
                })}
              </span>
            </div>
            <div className="usage-stat-grid">
              <div>
                <span className="usage-dot primary" />
                <small>{t("settings.publicImages")}</small>
                <strong>{stats?.publicCount ?? 0}</strong>
              </div>
              <div>
                <span className="usage-dot secondary" />
                <small>{t("settings.privateImages")}</small>
                <strong>
                  {(stats?.activeCount ?? 0) - (stats?.publicCount ?? 0)}
                </strong>
              </div>
              <div>
                <span className="usage-dot tertiary" />
                <small>{t("settings.activeImages")}</small>
                <strong>{stats?.activeCount ?? 0}</strong>
              </div>
              <div>
                <span className="usage-dot error" />
                <small>{t("settings.trash")}</small>
                <strong>{stats?.trashCount ?? 0}</strong>
              </div>
            </div>
          </div>
        </article>

        <div className="settings-save-row">
          <p>
            {t("settings.localNote")}
          </p>
          <button type="button" onClick={save}>
            <span className="material-symbols-outlined">save</span>
            {t("settings.save")}
          </button>
        </div>
      </div>
    </section>
  );
}
