export type ShareMode = "original" | "webp";

export interface NivalisPreferences {
  webpQuality: number;
  defaultShareMode: ShareMode;
}

const STORAGE_KEY = "nivalis-preferences";

export const DEFAULT_PREFERENCES: NivalisPreferences = {
  webpQuality: 85,
  defaultShareMode: "original",
};

export function loadPreferences(): NivalisPreferences {
  if (typeof window === "undefined") return DEFAULT_PREFERENCES;
  try {
    const saved = JSON.parse(
      window.localStorage.getItem(STORAGE_KEY) ?? "{}",
    ) as Partial<NivalisPreferences>;
    return {
      webpQuality:
        typeof saved.webpQuality === "number"
          ? Math.min(100, Math.max(1, Math.round(saved.webpQuality)))
          : DEFAULT_PREFERENCES.webpQuality,
      defaultShareMode:
        saved.defaultShareMode === "webp" ? "webp" : "original",
    };
  } catch {
    return DEFAULT_PREFERENCES;
  }
}

export function savePreferences(preferences: NivalisPreferences): void {
  window.localStorage.setItem(STORAGE_KEY, JSON.stringify(preferences));
}
