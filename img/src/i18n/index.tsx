import {
  useCallback,
  createContext,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import { en } from "./en";
import { ja } from "./ja";
import { zhCN } from "./zh-CN";
import { zhTW } from "./zh-TW";

export type Locale = "en" | "zh-CN" | "zh-TW" | "ja";
export type LocalePreference = "auto" | Locale;
export type MessageKey = keyof typeof en;
type Variables = Record<string, string | number>;

const dictionaries = { en, "zh-CN": zhCN, "zh-TW": zhTW, ja };
const LOCALE_STORAGE_KEY = "nivalis-locale";

export function detectLocale(languages?: readonly string[]): Locale {
  const candidates =
    languages ??
    (typeof navigator === "undefined"
      ? ["en"]
      : navigator.languages.length
        ? navigator.languages
        : [navigator.language]);
  for (const candidate of candidates) {
    const language = candidate.toLowerCase();
    if (
      language.startsWith("zh-hant") ||
      language.startsWith("zh-tw") ||
      language.startsWith("zh-hk") ||
      language.startsWith("zh-mo")
    ) {
      return "zh-TW";
    }
    if (language.startsWith("zh")) return "zh-CN";
    if (language.startsWith("ja")) return "ja";
    if (language.startsWith("en")) return "en";
  }
  return "en";
}

function isLocalePreference(value: string | null): value is LocalePreference {
  return (
    value === "auto" ||
    value === "en" ||
    value === "zh-CN" ||
    value === "zh-TW" ||
    value === "ja"
  );
}

export function loadLocalePreference(): LocalePreference {
  if (typeof window === "undefined") return "auto";
  const saved = window.localStorage.getItem(LOCALE_STORAGE_KEY);
  return isLocalePreference(saved) ? saved : "auto";
}

export function saveLocalePreference(preference: LocalePreference): void {
  window.localStorage.setItem(LOCALE_STORAGE_KEY, preference);
}

function interpolate(message: string, variables?: Variables): string {
  if (!variables) return message;
  return message.replace(/\{(\w+)\}/gu, (_, key: string) =>
    String(variables[key] ?? `{${key}}`),
  );
}

interface I18nValue {
  locale: Locale;
  localePreference: LocalePreference;
  setLocalePreference: (preference: LocalePreference) => void;
  t: (key: MessageKey, variables?: Variables) => string;
}

const I18nContext = createContext<I18nValue>({
  locale: "en",
  localePreference: "auto",
  setLocalePreference: () => undefined,
  t: (key, variables) => interpolate(en[key], variables),
});

export function I18nProvider({ children }: { children: ReactNode }) {
  const [localePreference, setLocalePreferenceState] =
    useState<LocalePreference>(loadLocalePreference);
  const locale = useMemo(
    () =>
      localePreference === "auto" ? detectLocale() : localePreference,
    [localePreference],
  );
  const setLocalePreference = useCallback((preference: LocalePreference) => {
    saveLocalePreference(preference);
    setLocalePreferenceState(preference);
  }, []);
  const value = useMemo<I18nValue>(
    () => ({
      locale,
      localePreference,
      setLocalePreference,
      t: (key, variables) =>
        interpolate(dictionaries[locale][key] ?? en[key], variables),
    }),
    [locale, localePreference, setLocalePreference],
  );

  useEffect(() => {
    document.documentElement.lang = locale;
  }, [locale]);

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n(): I18nValue {
  return useContext(I18nContext);
}
