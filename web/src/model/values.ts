import { ModelError } from "./errors";
import { joinListener, splitListener, type ListenerParts } from "./listener";
import type { SettingField } from "./settings";

export type RawFieldValue = string | boolean | ListenerParts;

export function fieldDisplayValue(value: unknown, field: SettingField): RawFieldValue {
  if (field.type === "checkbox") return value === true;
  if (field.type === "endpoint") return splitListener(typeof value === "string" ? value : "");
  if (Array.isArray(value)) return value.join("\n");
  return value === undefined || value === null ? "" : String(value);
}

/** Convert a touched field only. Untouched values should stay in the baseline verbatim. */
export function convertFieldValue(field: SettingField, draft: RawFieldValue): unknown {
  if (field.type === "endpoint") {
    if (typeof draft !== "object") throw new ModelError("settings.listener.address.required", field.path);
    try { return joinListener(draft.address, draft.port); }
    catch (error) {
      if (error instanceof ModelError) throw new ModelError(error.key, `${field.path}.${error.path}`);
      throw error;
    }
  }
  if (field.type === "checkbox") return draft === true;
  if (typeof draft !== "string") throw new ModelError("settings.validation.check", field.path);
  if (field.type === "lines") return draft.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
  if (field.path === "filter_file" || field.path.endsWith("_file")) return field.type === "nullable" && draft === "" ? null : draft;
  if (field.type === "nullable") return draft.trim() || null;
  if (field.type === "number") {
    const text = draft.trim();
    if (!/^[0-9]+$/.test(text) || !Number.isSafeInteger(Number(text))) {
      throw new ModelError("settings.validation.integer", field.path);
    }
    const value = Number(text);
    if ((field.min !== undefined && value < field.min) || (field.max !== undefined && value > field.max)) {
      throw new ModelError("settings.validation.range", field.path, { min: field.min ?? 0, max: field.max ?? 0 });
    }
    return value;
  }
  if (field.type === "select" && !field.options?.some(([value]) => value === draft)) {
    throw new ModelError("settings.validation.check", field.path);
  }
  if (field.type === "text" && draft.trim() === "" && !field.path.endsWith("_file")) {
    throw new ModelError("settings.validation.required", field.path);
  }
  return draft.trim();
}
