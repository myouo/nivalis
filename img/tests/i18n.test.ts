import { describe, expect, it } from "vitest";
import { detectLocale } from "../src/i18n";

describe("detectLocale", () => {
  it.each([
    [["zh-CN"], "zh-CN"],
    [["zh-Hans-SG"], "zh-CN"],
    [["zh-TW"], "zh-TW"],
    [["zh-Hant-HK"], "zh-TW"],
    [["ja-JP"], "ja"],
    [["en-US"], "en"],
    [["fr-FR", "ja-JP"], "ja"],
    [["fr-FR"], "en"],
  ] as const)("maps %j to %s", (languages, locale) => {
    expect(detectLocale(languages)).toBe(locale);
  });
});
