import { useEffect, useRef, useState } from 'react';
import { animate, useReducedMotion } from 'motion/react';
import { DURATION, EASE_OUT } from './beui/motion';

/**
 * Tweens a displayed number toward `value`. The first render counts up from
 * zero; later changes tween from the previous value, so polling never replays
 * an entrance. Reduced motion shows the value immediately.
 */
export function CountUp({ value, format }: { value: number; format: (value: number) => string }) {
  const reduced = useReducedMotion();
  const [shown, setShown] = useState(reduced ? value : 0);
  const from = useRef(reduced ? value : 0);
  useEffect(() => {
    if (reduced) { from.current = value; setShown(value); return; }
    const controls = animate(from.current, value, {
      duration: DURATION.count, ease: EASE_OUT,
      onUpdate: (next) => { from.current = next; setShown(next); },
    });
    return () => controls.stop();
  }, [value, reduced]);
  // Screen readers get the settled value once, not every intermediate frame.
  return <><span aria-hidden="true">{format(shown)}</span><span className="sr-only">{format(value)}</span></>;
}
