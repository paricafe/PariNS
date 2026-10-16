import { useEffect } from 'react';
import { AnimatePresence, motion, useReducedMotion } from 'motion/react';
import { effectiveOverlay, isPattern, type BackgroundId } from '../appearance/preferences';
import type { CustomImage } from '../appearance/useAppearance';

/**
 * Fixed, pointer-free page background shared by every screen. Each choice is its own layer
 * carrying data-bg, so the outgoing layer keeps its colours while the new one fades in
 * (opacity only). Patterns animate transforms only and are static under reduced motion.
 */
export function Backdrop({ background, image, overlay, dark }: { background: BackgroundId; image: CustomImage | null; overlay: number; dark: boolean }) {
  const reduced = useReducedMotion();
  const kind = background === 'custom' && image ? 'image' : isPattern(background) ? 'pattern' : 'solid';
  const shown: BackgroundId = background === 'custom' && !image ? 'neutral' : background;
  // Glass reads its fill strength from this: busier backdrops get a denser text backing.
  useEffect(() => { document.documentElement.dataset.backdrop = kind; }, [kind]);
  const key = shown === 'custom' ? `custom:${image?.url}` : shown;
  return <div className="backdrop" aria-hidden="true" data-static={reduced || undefined}>
    <AnimatePresence initial={false}>
      <motion.div key={key} className="backdrop-layer" data-bg={shown} initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}
        transition={{ duration: reduced ? 0 : 0.6, ease: 'easeOut' }}>
        {shown === 'orbs' && <><i className="orb" /><i className="orb" /><i className="orb" /></>}
        {shown === 'grid' && <i className="grid" />}
        {shown === 'gradient' && <i className="gradient" />}
        {shown === 'custom' && image && <>
          <i className="photo" style={{ backgroundImage: `url("${image.url}")` }} />
          <i className="scrim" style={{ opacity: effectiveOverlay(overlay, image.luminance, dark) / 100 }} />
        </>}
      </motion.div>
    </AnimatePresence>
  </div>;
}
