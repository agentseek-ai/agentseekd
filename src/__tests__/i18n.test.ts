import { describe, it, expect } from "vitest";
import { translations, type TranslationKey } from "../i18n";

const zhKeys = Object.keys(translations.zh) as TranslationKey[];
const enKeys = Object.keys(translations.en) as TranslationKey[];

describe("i18n translations", () => {
  it("zh and en have the same number of keys", () => {
    expect(zhKeys.length).toBe(enKeys.length);
  });

  it("every zh key exists in en", () => {
    for (const key of zhKeys) {
      expect(enKeys).toContain(key);
    }
  });

  it("every en key exists in zh", () => {
    for (const key of enKeys) {
      expect(zhKeys).toContain(key);
    }
  });

  it("every zh value is a non-empty string", () => {
    for (const key of zhKeys) {
      const value = translations.zh[key];
      expect(typeof value).toBe("string");
      expect(value.length).toBeGreaterThan(0);
    }
  });

  it("every en value is a non-empty string", () => {
    for (const key of enKeys) {
      const value = translations.en[key];
      expect(typeof value).toBe("string");
      expect(value.length).toBeGreaterThan(0);
    }
  });

  it("TranslationKey type matches zh keys", () => {
    // If this compiles, the type is correct
    const sampleKey: TranslationKey = "appSubtitle";
    expect(translations.zh[sampleKey]).toBeDefined();
    expect(translations.en[sampleKey]).toBeDefined();
  });
});
