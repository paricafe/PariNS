import { ModelError } from "./errors";

export type SettingsObject = Record<string, unknown>;

function isObject(value: unknown): value is SettingsObject {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

export function getPath(object: unknown, path: string): unknown {
  return path.split(".").reduce<unknown>((value, key) => isObject(value) ? value[key] : undefined, object);
}

/** Immutable nested write for a touched form field. */
export function setPath<T extends SettingsObject>(object: T, path: string, value: unknown): T {
  const keys = path.split(".");
  if (keys.some((key) => !key || key === "__proto__" || key === "prototype" || key === "constructor")) {
    throw new Error("Invalid settings path");
  }
  const copy: SettingsObject = { ...object };
  let target = copy;
  let source: unknown = object;
  for (const key of keys.slice(0, -1)) {
    source = isObject(source) ? source[key] : undefined;
    const child = isObject(source) ? { ...source } : {};
    target[key] = child;
    target = child;
  }
  target[keys[keys.length - 1]] = value;
  return copy as T;
}

function sameValue(left: unknown, right: unknown): boolean {
  if (Object.is(left, right)) return true;
  if (Array.isArray(left) && Array.isArray(right)) {
    return left.length === right.length && left.every((value, index) => sameValue(value, right[index]));
  }
  if (isObject(left) && isObject(right)) {
    const keys = Object.keys(left);
    return keys.length === Object.keys(right).length && keys.every((key) =>
      Object.prototype.hasOwnProperty.call(right, key) && sameValue(left[key], right[key]));
  }
  return false;
}

/** Only changed keys are sent to Rust preview; arrays and optional sections replace atomically. */
export function diffSettings(before: SettingsObject, after: SettingsObject): SettingsObject {
  const changes: SettingsObject = {};
  for (const [key, value] of Object.entries(after)) {
    const previous = before[key];
    if (sameValue(previous, value)) continue;
    changes[key] = isObject(previous) && isObject(value) ? diffSettings(previous, value) : value;
  }
  return changes;
}

/** Initial setup can only edit known keys in the supplied server template. */
export function networkTemplate(template: string, listen: string, upstreams: SettingsObject): string {
  const lines = template.split("\n");
  const changes = new Map<string, unknown>([["listen", listen], ...Object.entries(upstreams).map(([key, value]) => [`upstreams.${key}`, value] as const)]);
  let table = "";
  for (let index = 0; index < lines.length; index += 1) {
    const header = /^\s*\[([^\]]+)\]\s*$/.exec(lines[index]);
    if (header) { table = header[1]; continue; }
    const match = /^\s*(\w+)\s*=/.exec(lines[index]);
    const path = match && (table ? `${table}.${match[1]}` : match[1]);
    if (path && changes.has(path)) {
      lines[index] = `${match![1]} = ${JSON.stringify(changes.get(path))}`;
      changes.delete(path);
    }
  }
  if (changes.size) throw new ModelError("app.templateInvalid", undefined, { key: [...changes.keys()].join(", ") });
  return lines.join("\n");
}
