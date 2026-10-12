import { Minus, Plus } from 'lucide-react';
import { translate, type Language } from '../i18n';

/**
 * iOS-style stepper for small integer ranges. Typing keeps the raw draft for
 * Rust validation; − and + only step a clean integer within the field's
 * declared bounds, which mirror the Rust schema.
 */
export function Stepper({ id, label, value, min, max, disabled, language, onChange, describedBy }: {
  id: string; label: string; value: string; min: number; max: number; disabled?: boolean; language: Language; onChange(value: string): void; describedBy?: string;
}) {
  const current = /^\d+$/.test(value.trim()) ? Number(value.trim()) : null;
  const step = (delta: 1 | -1) => {
    const base = current ?? (delta > 0 ? min - 1 : max + 1);
    onChange(String(Math.min(max, Math.max(min, base + delta))));
  };
  return <div className="stepper">
    <input id={id} type="text" inputMode="numeric" value={value} disabled={disabled} spellCheck={false} autoComplete="off" aria-describedby={describedBy} onChange={(event) => onChange(event.target.value)} />
    <div className="stepper-buttons">
      <button type="button" aria-label={translate('shell.decrease', language, { label })} disabled={disabled || current !== null && current <= min} onClick={() => step(-1)}><Minus size={15} aria-hidden="true" /></button>
      <button type="button" aria-label={translate('shell.increase', language, { label })} disabled={disabled || current !== null && current >= max} onClick={() => step(1)}><Plus size={15} aria-hidden="true" /></button>
    </div>
  </div>;
}
