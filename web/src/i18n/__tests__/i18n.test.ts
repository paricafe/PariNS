import { describe, expect, it } from "vitest";
import { chooseLanguage, hasTranslation, normalizeLanguage, translate, translationKeys } from "../index";
import { apiMessages, appMessages, uiMessages } from "../console";
import { settingsMessages } from "../settings";
import { viewsMessages } from "../views";
import { storageMessages } from '../storage';
import { reliabilityMessages } from '../reliability';
import { settingPages } from "../../model";

describe("bilingual console catalog", () => {
  it("includes both languages and matching interpolation fields", () => {
    const catalogs = [apiMessages, appMessages, uiMessages, settingsMessages, viewsMessages, storageMessages, reliabilityMessages];
    for (const entries of catalogs) for (const [key, pair] of Object.entries(entries)) {
      expect(pair[0], key).toBeTruthy();
      expect(pair[1], key).toBeTruthy();
      expect(pair[1], key).not.toMatch(/[\u3400-\u9fff]/u);
      const fields = (text: string) => [...text.matchAll(/\{(\w+)\}/g)].map((match) => match[1]).sort();
      expect(fields(pair[0]), key).toEqual(fields(pair[1]));
    }
  });

  it("resolves all settings descriptors in both languages", () => {
    for (const page of Object.values(settingPages)) {
      const keys = [page.titleKey, page.introKey];
      for (const group of page.groups) {
        keys.push(group.titleKey, group.helpKey);
        if (group.optional) keys.push(group.enableKey);
        for (const field of group.fields) {
          keys.push(field.labelKey);
          if (field.helpKey) keys.push(field.helpKey);
          for (const [, key] of field.options ?? []) keys.push(key);
        }
      }
      for (const key of keys) {
        expect(hasTranslation(key), key).toBe(true);
        expect(translate(key, "en"), key).not.toBe(key);
      }
    }
    expect(translationKeys.length).toBeGreaterThan(400);
  });

  it("chooses saved language, then supported browser language", () => {
    expect(chooseLanguage("en", ["zh-TW"])).toBe("en");
    expect(chooseLanguage(null, ["fr", "en-GB"])).toBe("en");
    expect(chooseLanguage(null, ["zh-TW"])).toBe("zh-CN");
    expect(chooseLanguage(null, ["fr"])).toBe("zh-CN");
    expect(normalizeLanguage("en-US")).toBe("en");
  });

  it("interpolates as text and keeps missing keys visible", () => {
    expect(translate("app.context", "en", { page: "<script>$&</script>" }))
      .toBe("Console / <script>$&</script>");
    expect(translate("missing.key", "zh-CN")).toBe("missing.key");
  });

  it("covers secure session and appearance states without obsolete refresh copy", () => {
    for (const key of [
      "api.SESSION_CHANGED", "api.TRANSPORT_CHANGED", "app.cookieUnavailable", "app.transportChanged",
      "app.logoutUnknown", "app.sessionChanged", "ui.theme", "ui.themeAuto",
      "ui.themeLight", "ui.themeDark",
    ]) expect(hasTranslation(key), key).toBe(true);
    expect(translate("ui.sessionHelp", "zh-CN")).not.toContain("需要重新登录");
    expect(translate("ui.sessionHelp", "en")).toContain("stays signed in");
  });
});
