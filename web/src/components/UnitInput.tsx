import { useEffect, useRef, useState } from 'react';
import { translate, type Language } from '../i18n';
import { Segmented } from './Segmented';

/** Presentation-only unit families; the draft always holds the base unit Rust expects. */
export type UnitFamily = 'bytes' | 'milliseconds' | 'seconds';
const UNITS: Record<UnitFamily, readonly (readonly [key: string, factor: number])[]> = {
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

/** Largest unit that represents the stored base value exactly. */
function initial(family: UnitFamily, raw: string): { factor: number; text: string } {
  const base = /^\d+$/.test(raw.trim()) ? Number(raw.trim()) : null;
  if (base === null || base === 0) return { factor: 1, text: raw };
  const [, factor] = [...UNITS[family]].reverse().find(([, candidate]) => base % candidate === 0) ?? UNITS[family][0];
  return { factor, text: String(base / factor) };
}

/**
 * Converts a clean decimal to an exact integer base value; anything else stays a raw draft for validation.
 * Decimal arithmetic avoids float error, so 0.035 hours is exactly 126 seconds.
 */
function toBase(text: string, factor: number): string {
  const match = /^(\d+)(?:\.(\d+))?$/.exec(text.trim());
  if (!match) return text;
  const fraction = match[2] ?? '';
  const scaled = BigInt(match[1] + fraction) * BigInt(factor);
  const divisor = 10n ** BigInt(fraction.length);
  if (scaled % divisor !== 0n) return text;
  const base = scaled / divisor;
  return base <= BigInt(Number.MAX_SAFE_INTEGER) ? String(base) : text;
}

export function UnitInput({ id, label, family, value, disabled, language, onChange, describedBy }: {
  id: string; label: string; family: UnitFamily; value: string; disabled?: boolean; language: Language; onChange(value: string): void; describedBy?: string;
}) {
  const [state, setState] = useState(() => initial(family, value));
  const emitted = useRef(value);
  // External changes (load, discard, preview) re-derive the display; our own edits do not.
  useEffect(() => { if (value !== emitted.current) { emitted.current = value; setState(initial(family, value)); } }, [family, value]);
  const emit = (text: string, factor: number) => {
    setState({ text, factor });
    const next = toBase(text, factor);
    emitted.current = next;
    onChange(next);
  };
  return <div className="unit-input">
    <input id={id} type="text" inputMode="decimal" value={state.text} disabled={disabled} spellCheck={false} autoComplete="off" aria-describedby={describedBy}
      onChange={(event) => emit(event.target.value, state.factor)} />
    <Segmented label={`${label} · ${translate('shell.unit', language)}`} value={String(state.factor)} disabled={disabled}
      options={UNITS[family].map(([key, factor]) => ({ value: String(factor), label: translate(`shell.unit_${key}`, language) }))}
      onChange={(factor) => emit(state.text, Number(factor))} />
  </div>;
}
