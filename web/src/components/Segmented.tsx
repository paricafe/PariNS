import { useId, useRef, type KeyboardEvent } from 'react';
import { motion, useReducedMotion } from 'motion/react';
import { SPRING_IOS_FAST } from './beui/motion';

export interface SegmentOption { value: string; label: string }

/** iOS segmented control for choosing a value (radio semantics); shares the tab track styling. */
export function Segmented({ id, value, options, onChange, label, disabled, className }: {
  id?: string; value: string; options: readonly SegmentOption[]; onChange(value: string): void; label: string; disabled?: boolean; className?: string;
}) {
  const thumb = useId();
  const reduced = useReducedMotion();
  const buttons = useRef<Array<HTMLButtonElement | null>>([]);
  const selected = options.findIndex((option) => option.value === value);
  const onKeyDown = (event: KeyboardEvent<HTMLButtonElement>, index: number) => {
    const delta = event.key === 'ArrowRight' || event.key === 'ArrowDown' ? 1 : event.key === 'ArrowLeft' || event.key === 'ArrowUp' ? -1 : 0;
    if (!delta) return;
    event.preventDefault();
    const next = (index + delta + options.length) % options.length;
    buttons.current[next]?.focus();
    onChange(options[next].value);
  };
  return <div id={id} role="radiogroup" aria-label={label} aria-disabled={disabled || undefined} className={`beui-tabs segmented ${className ?? ''}`}>
    {options.map((option, index) => {
      const checked = index === selected;
      return <button key={option.value} ref={(node) => { buttons.current[index] = node; }} type="button" role="radio" aria-checked={checked}
        tabIndex={checked || selected < 0 && index === 0 ? 0 : -1} disabled={disabled} className="beui-tab"
        onClick={() => { if (!checked) onChange(option.value); }} onKeyDown={(event) => onKeyDown(event, index)}>
        {option.label}
        {checked && <motion.span aria-hidden="true" layoutId={thumb} className="beui-tab__indicator" transition={reduced ? { duration: 0 } : SPRING_IOS_FAST} />}
      </button>;
    })}
  </div>;
}
