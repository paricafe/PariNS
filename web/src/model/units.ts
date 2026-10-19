/** Presentation-only unit families; stored values are always the base unit Rust expects. */
export type UnitFamily = 'bytes' | 'milliseconds' | 'seconds';
export const UNITS: Record<UnitFamily, readonly (readonly [key: string, factor: number])[]> = {
  bytes: [['bytes', 1], ['kib', 1024], ['mib', 1024 ** 2], ['gib', 1024 ** 3]],
  milliseconds: [['ms', 1], ['seconds', 1000], ['minutes', 60_000]],
  seconds: [['seconds', 1], ['minutes', 60], ['hours', 3600], ['days', 86_400]],
};

export function unitFamily(path: string): UnitFamily | null {
  if (path.endsWith('_bytes')) return 'bytes';
  if (path.endsWith('_ms')) return 'milliseconds';
  if (path.endsWith('_secs')) return 'seconds';
  return null;
}

export function isUnitFamily(value: unknown): value is UnitFamily {
  return value === 'bytes' || value === 'milliseconds' || value === 'seconds';
}

/** The largest unit that represents a nonzero base value exactly. */
export function exactUnit(family: UnitFamily, base: number): readonly [key: string, factor: number] {
  return [...UNITS[family]].reverse().find(([, factor]) => base % factor === 0) ?? UNITS[family][0];
}
