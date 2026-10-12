import { useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { AnimatePresence, motion, useReducedMotion } from 'motion/react';
import { Check, ChevronsUpDown } from 'lucide-react';
import { SPRING_IOS_FAST } from './beui/motion';

/** Closes on outside pointer-down; Escape is handled by each control so focus can return. */
function useDismiss(open: boolean, root: React.RefObject<HTMLElement | null>, close: () => void) {
  useEffect(() => {
    if (!open) return;
    const down = (event: PointerEvent) => { if (!root.current?.contains(event.target as Node)) close(); };
    document.addEventListener('pointerdown', down);
    return () => document.removeEventListener('pointerdown', down);
  }, [open, root, close]);
}

function Pop({ open, children, align = 'start', role, id, label, onKeyDown }: { open: boolean; children: ReactNode; align?: 'start' | 'end'; role: 'listbox' | 'menu'; id: string; label: string; onKeyDown(event: KeyboardEvent<HTMLElement>): void }) {
  const reduced = useReducedMotion();
  return <AnimatePresence>{open && <motion.div key="pop" id={id} role={role} aria-label={label} tabIndex={-1} className="popover" data-align={align} onKeyDown={onKeyDown}
    initial={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.92, y: -4 }} animate={{ opacity: 1, scale: 1, y: 0 }}
    exit={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.96 }} transition={reduced ? { duration: 0.12 } : SPRING_IOS_FAST}>
    {children}
  </motion.div>}</AnimatePresence>;
}

function nextIndex(key: string, current: number, count: number): number | null {
  switch (key) {
    case 'ArrowDown': return (current + 1) % count;
    case 'ArrowUp': return (current - 1 + count) % count;
    case 'Home': return 0;
    case 'End': return count - 1;
    default: return null;
  }
}

export interface SelectOption { value: string; label: string }

/** An iOS-style popup menu standing in for a native <select>; a listbox keeps select semantics. */
export function Select({ id, value, options, onChange, label, disabled, className }: {
  id?: string; value: string; options: readonly SelectOption[]; onChange(value: string): void; label: string; disabled?: boolean; className?: string;
}) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const items = useRef<Array<HTMLDivElement | null>>([]);
  const listId = useId();
  const selected = Math.max(0, options.findIndex((option) => option.value === value));
  const close = () => setOpen(false);
  useDismiss(open, root, close);
  useEffect(() => { if (open) items.current[active]?.focus(); }, [open, active]);
  const show = (at = selected) => { if (disabled) return; setActive(at); setOpen(true); };
  const choose = (at: number) => { setOpen(false); button.current?.focus(); if (options[at] && options[at].value !== value) onChange(options[at].value); };
  const onButtonKey = (event: KeyboardEvent<HTMLButtonElement>) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') { event.preventDefault(); show(); }
  };
  const onListKey = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); close(); button.current?.focus(); return; }
    if (event.key === 'Tab') { close(); return; }
    if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); choose(active); return; }
    const next = nextIndex(event.key, active, options.length);
    if (next !== null) { event.preventDefault(); setActive(next); }
  };
  return <div ref={root} className={`select-control ${className ?? ''}`}>
    <button ref={button} id={id} type="button" className="select-button" disabled={disabled} aria-haspopup="listbox" aria-expanded={open} aria-controls={open ? listId : undefined}
      aria-label={`${label}: ${options[selected]?.label ?? ''}`} onClick={() => open ? close() : show()} onKeyDown={onButtonKey}>
      <span>{options[selected]?.label ?? '—'}</span><ChevronsUpDown size={14} aria-hidden="true" />
    </button>
    <Pop open={open} role="listbox" id={listId} label={label} onKeyDown={onListKey}>
      {options.map((option, index) => <div key={option.value} ref={(node) => { items.current[index] = node; }} role="option" tabIndex={-1}
        aria-selected={index === selected} className="popover-item" data-active={index === active || undefined}
        onPointerMove={() => setActive(index)} onClick={() => choose(index)}>
        <span className="popover-check" aria-hidden="true">{index === selected && <Check size={15} />}</span>{option.label}
      </div>)}
    </Pop>
  </div>;
}

export interface MenuAction { label: string; onSelect(trigger: HTMLElement): void; disabled?: boolean; destructive?: boolean }

/** Action menu ("More"); disabled actions are omitted so only usable actions are offered. */
export function Menu({ label, icon, actions, align = 'end' }: { label: string; icon: ReactNode; actions: readonly MenuAction[]; align?: 'start' | 'end' }) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const items = useRef<Array<HTMLButtonElement | null>>([]);
  const menuId = useId();
  const available = actions.filter((action) => !action.disabled);
  const close = () => setOpen(false);
  useDismiss(open, root, close);
  useEffect(() => { if (open) items.current[active]?.focus(); }, [open, active]);
  // While nothing can run (for example during a save) the button stays focusable but inert, so focus is never dropped.
  const inert = !available.length;
  const run = (action: MenuAction) => { setOpen(false); const trigger = button.current!; trigger.focus(); action.onSelect(trigger); };
  const onKey = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); close(); button.current?.focus(); return; }
    if (event.key === 'Tab') { close(); return; }
    const next = nextIndex(event.key, active, available.length);
    if (next !== null) { event.preventDefault(); setActive(next); }
  };
  return <div ref={root} className="select-control">
    <button ref={button} type="button" className="button secondary menu-button" aria-haspopup="menu" aria-expanded={open && !inert} aria-controls={open && !inert ? menuId : undefined} aria-disabled={inert || undefined}
      onClick={() => { if (inert) return; setActive(0); setOpen(!open); }} onKeyDown={(event) => { if (event.key === 'ArrowDown' && !inert) { event.preventDefault(); setActive(0); setOpen(true); } }}>
      {icon}<span>{label}</span>
    </button>
    <Pop open={open && !inert} role="menu" id={menuId} label={label} align={align} onKeyDown={onKey}>
      {available.map((action, index) => <button key={action.label} ref={(node) => { items.current[index] = node; }} type="button" role="menuitem" tabIndex={-1}
        className="popover-item" data-destructive={action.destructive || undefined} data-active={index === active || undefined}
        onPointerMove={() => setActive(index)} onClick={() => run(action)}>{action.label}</button>)}
    </Pop>
  </div>;
}
