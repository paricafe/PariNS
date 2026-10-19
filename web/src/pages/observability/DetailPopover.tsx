import { useLayoutEffect, useRef, type KeyboardEvent, type PointerEvent, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { motion, useReducedMotion } from 'motion/react';
import { SPRING_IOS_FAST } from '../../components/beui/motion';

const MARGIN = 8;
const GAP = 6;
const MAX_WIDTH = 440;

/** Places the card beside its anchor in viewport coordinates: below unless above has more room, clamped to the viewport. */
function place(card: HTMLElement, anchor: HTMLElement) {
  const box = anchor.getBoundingClientRect();
  const width = document.documentElement.clientWidth;
  const height = window.innerHeight;
  const cardWidth = Math.min(MAX_WIDTH, width - 2 * MARGIN);
  const below = height - box.bottom - GAP - MARGIN;
  const above = box.top - GAP - MARGIN;
  const up = card.scrollHeight > below && above > below;
  const left = Math.min(Math.max(box.right - cardWidth, MARGIN), width - MARGIN - cardWidth);
  Object.assign(card.style, {
    width: `${cardWidth}px`, left: `${left}px`, maxHeight: `${Math.max(up ? above : below, 0)}px`,
    top: up ? '' : `${box.bottom + GAP}px`, bottom: up ? `${height - box.top + GAP}px` : '',
    transformOrigin: `${up ? 'bottom' : 'top'} ${box.right - left > cardWidth / 2 ? 'right' : 'left'}`,
  });
}

/**
 * A glass card in a body portal, so table overflow never clips it. It follows its
 * anchor on scroll and resize; the owner handles opening and closing. With `focus`
 * (opened from the keyboard) the scrolling body takes focus so arrow keys scroll it.
 */
export function DetailPopover({ id, label, anchor, focus, onPointerEnter, onPointerLeave, onPointerDown, onKeyDown, children }: {
  id: string; label: string; anchor: HTMLElement; focus: boolean; children: ReactNode;
  onPointerEnter(event: PointerEvent): void; onPointerLeave(event: PointerEvent): void; onPointerDown(): void; onKeyDown(event: KeyboardEvent<HTMLElement>): void;
}) {
  const reduced = useReducedMotion();
  const card = useRef<HTMLDivElement>(null);
  const body = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => { if (focus) body.current?.focus(); }, [focus]);
  useLayoutEffect(() => {
    const update = () => { if (card.current) place(card.current, anchor); };
    update();
    window.addEventListener('scroll', update, true);
    window.addEventListener('resize', update);
    return () => { window.removeEventListener('scroll', update, true); window.removeEventListener('resize', update); };
  }, [anchor]);
  return createPortal(<motion.div ref={card} id={id} role="dialog" aria-label={label} className="popover log-popover"
    onPointerEnter={onPointerEnter} onPointerLeave={onPointerLeave} onPointerDown={onPointerDown} onKeyDown={onKeyDown}
    initial={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.94 }} animate={{ opacity: 1, scale: 1 }}
    exit={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.97 }} transition={reduced ? { duration: 0.12 } : SPRING_IOS_FAST}>
    <div ref={body} tabIndex={-1} className="log-popover-body">{children}</div>
  </motion.div>, document.body);
}
