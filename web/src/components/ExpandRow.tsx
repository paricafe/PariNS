import { useId, useState, type ReactNode } from 'react';
import { motion, useReducedMotion } from 'motion/react';
import { ChevronRight } from 'lucide-react';
import { SPRING_IOS_FAST } from './beui/motion';

/** A grouped-list row with “›” that expands its content in place; the content stays mounted but inert while closed. */
export function ExpandRow({ label, children, open: controlled, onToggle }: { label: string; children: ReactNode; open?: boolean; onToggle?(open: boolean): void }) {
  const [local, setLocal] = useState(false);
  const open = controlled ?? local;
  const reduced = useReducedMotion();
  const id = useId();
  return <>
    <button type="button" className="row row-disclosure" aria-expanded={open} aria-controls={id} onClick={() => { setLocal(!open); onToggle?.(!open); }}>
      <span>{label}</span><motion.span className="chevron" aria-hidden="true" animate={{ rotate: open ? 90 : 0 }} transition={reduced ? { duration: 0 } : SPRING_IOS_FAST}><ChevronRight size={17} /></motion.span>
    </button>
    <div id={id} className="settings-expand" data-open={open} inert={!open}><div>{children}</div></div>
  </>;
}
