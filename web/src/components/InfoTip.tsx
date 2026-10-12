import { useEffect, useId, useRef, useState } from 'react';
import { AnimatePresence, motion, useReducedMotion } from 'motion/react';
import { Info } from 'lucide-react';
import { SPRING_IOS_FAST } from './beui/motion';

/** ⓘ button that reveals a field explanation on demand; the text stays in the DOM order after the button. */
export function InfoTip({ text, label }: { text: string; label: string }) {
  const [open, setOpen] = useState(false);
  const reduced = useReducedMotion();
  const root = useRef<HTMLSpanElement>(null);
  const id = useId();
  useEffect(() => {
    if (!open) return;
    const down = (event: PointerEvent) => { if (!root.current?.contains(event.target as Node)) setOpen(false); };
    document.addEventListener('pointerdown', down);
    return () => document.removeEventListener('pointerdown', down);
  }, [open]);
  return <span ref={root} className="info-tip">
    <button type="button" className="info-button" aria-label={label} aria-expanded={open} aria-controls={id}
      onClick={() => setOpen(!open)} onKeyDown={(event) => { if (event.key === 'Escape' && open) { event.stopPropagation(); setOpen(false); } }}>
      <Info size={15} aria-hidden="true" />
    </button>
    <AnimatePresence>{open && <motion.span id={id} role="note" className="info-bubble"
      initial={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.94, y: -4 }} animate={{ opacity: 1, scale: 1, y: 0 }}
      exit={{ opacity: 0 }} transition={reduced ? { duration: 0.12 } : SPRING_IOS_FAST}>{text}</motion.span>}</AnimatePresence>
  </span>;
}
