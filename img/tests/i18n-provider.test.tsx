import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import {
  I18nProvider,
  loadLocalePreference,
  useI18n,
} from "../src/i18n";

function LocaleProbe() {
  const { locale, localePreference, setLocalePreference, t } = useI18n();
  return (
    <div>
      <span data-testid="locale">{locale}</span>
      <span data-testid="preference">{localePreference}</span>
      <span data-testid="settings-title">{t("settings.title")}</span>
      <button type="button" onClick={() => setLocalePreference("ja")}>
        Japanese
      </button>
    </div>
  );
}

describe("I18nProvider preferences", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("falls back to automatic mode for an invalid saved value", () => {
    window.localStorage.setItem("nivalis-locale", "invalid");
    expect(loadLocalePreference()).toBe("auto");
  });

  it("applies and persists an explicit language", () => {
    render(
      <I18nProvider>
        <LocaleProbe />
      </I18nProvider>,
    );

    fireEvent.click(screen.getByRole("button", { name: "Japanese" }));

    expect(screen.getByTestId("locale")).toHaveTextContent("ja");
    expect(screen.getByTestId("preference")).toHaveTextContent("ja");
    expect(screen.getByTestId("settings-title")).toHaveTextContent(
      "Nivalis 設定",
    );
    expect(document.documentElement.lang).toBe("ja");
    expect(window.localStorage.getItem("nivalis-locale")).toBe("ja");
  });
});
