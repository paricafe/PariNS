import { apiMessages, appMessages, uiMessages } from "./console";
import { settingsMessages } from "./settings";
import { viewsMessages } from "./views";
import { storageMessages } from "./storage";
import { reliabilityMessages } from './reliability';
import { ModelError } from "../model/errors";
import { ApiError } from "../session/client";

export type Language = "zh-CN" | "en";
export type TranslationParams = Readonly<Record<string, string | number | boolean | null | undefined>>;
type MessagePair = readonly [string, string];

const catalog: Record<string, MessagePair> = {
  ...Object.fromEntries(Object.entries(apiMessages).map(([key, value]) => [`api.${key}`, value])),
  ...Object.fromEntries(Object.entries(appMessages).map(([key, value]) => [`app.${key}`, value])),
  ...Object.fromEntries(Object.entries(uiMessages).map(([key, value]) => [`ui.${key}`, value])),
  ...Object.fromEntries(Object.entries(settingsMessages).map(([key, value]) => [`settings.${key}`, value])),
  ...Object.fromEntries(Object.entries(viewsMessages).map(([key, value]) => [`views.${key}`, value])),
  ...Object.fromEntries(Object.entries(storageMessages).map(([key, value]) => [`storage.${key}`, value])),
  ...Object.fromEntries(Object.entries(reliabilityMessages).map(([key, value]) => [`reliability.${key}`, value])),
};

export function translate(key: string, language: Language, params: TranslationParams = {}): string {
  const template = catalog[key]?.[language === "en" ? 1 : 0] ?? key;
  return template.replace(/\{(\w+)\}/g, (match, name: string) =>
    Object.prototype.hasOwnProperty.call(params, name) ? String(params[name]) : match,
  );
}

export function hasTranslation(key: string): boolean {
  return Object.prototype.hasOwnProperty.call(catalog, key);
}

export function presentIssue(issue: string | ModelError | ApiError, language: Language): string {
  if (issue instanceof ApiError) {
    const key = `api.${issue.code}`;
    return hasTranslation(key) ? translate(key, language, { detail: issue.message, status: issue.status }) : issue.message;
  }
  const key = issue instanceof ModelError ? issue.key : issue;
  const params = issue instanceof ModelError ? issue.params : {};
  return hasTranslation(key) ? translate(key, language, params) : key;
}

export function normalizeLanguage(value: string | null | undefined): Language | null {
  if (/^zh(?:-|$)/i.test(value ?? "")) return "zh-CN";
  if (/^en(?:-|$)/i.test(value ?? "")) return "en";
  return null;
}

export function chooseLanguage(saved: string | null | undefined, browserLanguages: readonly string[]): Language {
  return normalizeLanguage(saved) ?? browserLanguages.map(normalizeLanguage).find((language) => language !== null) ?? "zh-CN";
}

export function formatNumber(value: number, language: Language, options?: Intl.NumberFormatOptions): string {
  return new Intl.NumberFormat(language, options).format(value);
}

export function formatDate(value: string | number | Date, language: Language, options?: Intl.DateTimeFormatOptions): string {
  return new Intl.DateTimeFormat(language, options).format(new Date(value));
}

export const translationKeys = Object.freeze(Object.keys(catalog));
