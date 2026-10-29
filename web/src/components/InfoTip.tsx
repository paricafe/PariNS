import { useEffect, useId, useLayoutEffect, useRef, useState } from 'react';
import { AnimatePresence, motion, useReducedMotion } from 'motion/react';
import { Info } from 'lucide-react';
import { SPRING_IOS_FAST } from './beui/motion';

/** ⓘ button that reveals a field explanation on demand; the text stays in the DOM order after the button. */
export function InfoTip({ text, label }: { text: string; label: string }) {
  const [open, setOpen] = useState(false);
  const reduced = useReducedMotion();
  const root = useRef<HTMLSpanElement>(null);
  const bubble = useRef<HTMLSpanElement>(null);
  const [left, setLeft] = useState(-8);
  const id = useId();
  useLayoutEffect(() => {
    if (!open) return;
    const place = () => {
      if (!root.current || !bubble.current) return;
      const anchor = root.current.getBoundingClientRect();
      const right = document.documentElement.clientWidth - 8;
      setLeft(Math.max(8 - anchor.left, Math.min(-8, right - anchor.left - bubble.current.offsetWidth)));
    };
    place();
    window.addEventListener('resize', place);
    return () => window.removeEventListener('resize', place);
  }, [open, text]);
  useEffect(() => {
    if (!open) return;
    const down = (event: PointerEvent) => { if (!root.current?.contains(event.target as Node)) setOpen(false); };
    // Escape closes the open tip from any focus position, before an enclosing dialog or card sees it.
    const key = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      event.preventDefault(); event.stopPropagation(); setOpen(false);
    };
    document.addEventListener('pointerdown', down);
    document.addEventListener('keydown', key, true);
    return () => { document.removeEventListener('pointerdown', down); document.removeEventListener('keydown', key, true); };
  }, [open]);
  return <span ref={root} className="info-tip">
    <button type="button" className="info-button" aria-label={label} aria-expanded={open} aria-controls={id}
      onClick={() => setOpen(!open)}>
      <Info size={15} aria-hidden="true" />
    </button>
    <AnimatePresence>{open && <motion.span ref={bubble} id={id} role="note" className="info-bubble" style={{ left }}
      initial={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.94, y: -4 }} animate={{ opacity: 1, scale: 1, y: 0 }}
      exit={{ opacity: 0 }} transition={reduced ? { duration: 0.12 } : SPRING_IOS_FAST}>{text}</motion.span>}</AnimatePresence>
  </span>;
}
