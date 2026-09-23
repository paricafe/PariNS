import { ModelError } from "./errors";

export interface CacheRule {
  name: string;
  suffix: boolean;
  qtype: string | null;
  bypass: boolean;
  max_ttl_secs: number | null;
  negative_ttl_cap_secs: number | null;
  prefetch: boolean | null;
  stale: boolean | null;
}

/** Each input stays a string so incomplete edits survive navigation and translation. */
export type CacheRuleDraft = Record<keyof CacheRule, string>;

export function defaultCacheRule(): CacheRule {
  return {
    name: "", suffix: false, qtype: null, bypass: false,
    max_ttl_secs: null, negative_ttl_cap_secs: null, prefetch: null, stale: null,
  };
}

export function cacheRuleDraft(rule: CacheRule): CacheRuleDraft {
  return {
    name: rule.name,
    suffix: String(rule.suffix),
    qtype: rule.qtype ?? "",
    bypass: String(rule.bypass),
    max_ttl_secs: rule.max_ttl_secs === null ? "" : String(rule.max_ttl_secs),
    negative_ttl_cap_secs: rule.negative_ttl_cap_secs === null ? "" : String(rule.negative_ttl_cap_secs),
    prefetch: rule.prefetch === null ? "" : String(rule.prefetch),
    stale: rule.stale === null ? "" : String(rule.stale),
  };
}

function ttl(value: string, field: "max_ttl_secs" | "negative_ttl_cap_secs"): number | null {
  const text = value.trim();
  if (!text) return null;
  if (!/^[0-9]+$/.test(text) || !Number.isSafeInteger(Number(text)) || Number(text) < 1 || Number(text) > 86400) {
    throw new ModelError("views.ttlError", field);
  }
  return Number(text);
}

function optionalBoolean(value: string, field: "prefetch" | "stale"): boolean | null {
  if (value === "") return null;
  if (value === "true") return true;
  if (value === "false") return false;
  throw new ModelError("settings.validation.check", field);
}

/** Convert only for preview; DNS rule validation and matching remain Rust-owned. */
export function decodeCacheRule(draft: CacheRuleDraft): CacheRule {
  const name = draft.name.trim();
  if (!name) throw new ModelError("views.domainError", "name");
  if (draft.suffix !== "true" && draft.suffix !== "false") throw new ModelError("settings.validation.check", "suffix");
  if (draft.bypass !== "true" && draft.bypass !== "false") throw new ModelError("settings.validation.check", "bypass");
  return {
    name,
    suffix: draft.suffix === "true",
    qtype: draft.qtype.trim().toUpperCase() || null,
    bypass: draft.bypass === "true",
    max_ttl_secs: ttl(draft.max_ttl_secs, "max_ttl_secs"),
    negative_ttl_cap_secs: ttl(draft.negative_ttl_cap_secs, "negative_ttl_cap_secs"),
    prefetch: optionalBoolean(draft.prefetch, "prefetch"),
    stale: optionalBoolean(draft.stale, "stale"),
  };
}
