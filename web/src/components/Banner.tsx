import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from 'react';
import { AnimatePresence, motion, useReducedMotion } from 'motion/react';
import { CheckCircle2 } from 'lucide-react';
import { SPRING_IOS } from './beui/motion';

type Show = (message: string) => void;
const BannerContext = createContext<Show>(() => {});

/**
 * Transient success feedback (iOS banner). The live region always exists so
 * screen readers announce each message; failures stay inline where they occurred.
 */
export function BannerProvider({ children }: { children: ReactNode }) {
  const reduced = useReducedMotion();
  const [banner, setBanner] = useState<{ id: number; message: string } | null>(null);
  const next = useRef(0);
  const show = useCallback<Show>((message) => setBanner({ id: ++next.current, message }), []);
  useEffect(() => {
    if (!banner) return;
    const timer = window.setTimeout(() => setBanner((current) => current?.id === banner.id ? null : current), 2400);
    return () => window.clearTimeout(timer);
  }, [banner]);
  return <BannerContext.Provider value={show}>
    {children}
    <div className="banner-region" role="status" aria-live="polite">
      <AnimatePresence>{banner && <motion.div key={banner.id} className="banner"
        initial={reduced ? { opacity: 0 } : { opacity: 0, y: -48, scale: 0.96 }} animate={{ opacity: 1, y: 0, scale: 1 }}
        exit={reduced ? { opacity: 0 } : { opacity: 0, y: -32, scale: 0.98 }} transition={reduced ? { duration: 0.15 } : SPRING_IOS}>
        <CheckCircle2 size={18} aria-hidden="true" /><span>{banner.message}</span>
      </motion.div>}</AnimatePresence>
    </div>
  </BannerContext.Provider>;
}

export function useBanner(): Show { return useContext(BannerContext); }
