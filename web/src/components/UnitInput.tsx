import { useEffect, useRef, useState } from 'react';
import { translate, type Language } from '../i18n';
import { Segmented } from './Segmented';
import { UNITS, exactUnit, unitFamily, type UnitFamily } from '../model/units';

export { unitFamily, type UnitFamily };

/** Largest unit that represents the stored base value exactly. */
function initial(family: UnitFamily, raw: string): { factor: number; text: string } {
  const base = /^\d+$/.test(raw.trim()) ? Number(raw.trim()) : null;
  if (base === null || base === 0) return { factor: 1, text: raw };
  const [, factor] = exactUnit(family, base);
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

/** Units with one unit at most `max`, so a unit cannot invite a value over the limit; the one in use always stays. */
function offeredUnits(family: UnitFamily, max: number | undefined, current: number) {
  return UNITS[family].filter(([, factor]) => max === undefined || factor <= max || factor === current);
}

export function UnitInput({ id, label, family, max, value, disabled, language, onChange, describedBy }: {
  id: string; label: string; family: UnitFamily; max?: number; value: string; disabled?: boolean; language: Language; onChange(value: string): void; describedBy?: string;
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
      options={offeredUnits(family, max, state.factor).map(([key, factor]) => ({ value: String(factor), label: translate(`shell.unit_${key}`, language) }))}
      onChange={(factor) => emit(state.text, Number(factor))} />
  </div>;
}
