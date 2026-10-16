import { AnimatePresence, motion, useReducedMotion } from 'motion/react';
import { Monitor, Moon, Sun } from 'lucide-react';
import { translate, type Language } from '../i18n';
import { nextTheme, type Theme } from '../appearance/preferences';
import { SPRING_IOS_FAST } from './beui/motion';

const ICONS = { system: Monitor, light: Sun, dark: Moon } as const;
const LABELS = { system: 'ui.themeAuto', light: 'ui.themeLight', dark: 'ui.themeDark' } as const;

export const themeName = (theme: Theme, language: Language) => translate(LABELS[theme], language);

/** One top-bar button cycling system → light → dark; its name always states the current theme. */
export function ThemeButton({ theme, language, onChange }: { theme: Theme; language: Language; onChange(theme: Theme): void }) {
  const reduced = useReducedMotion();
  const Icon = ICONS[theme];
  const state = themeName(theme, language);
  const next = nextTheme(theme);
  return <button type="button" className="icon-button theme-button" data-theme-state={theme}
    aria-label={translate('appearance.theme', language, { state })}
    title={translate('appearance.themeNext', language, { state, next: themeName(next, language) })}
    onClick={() => onChange(next)}>
    <AnimatePresence mode="popLayout" initial={false}>
      <motion.span key={theme} className="theme-button-icon" aria-hidden="true"
        initial={reduced ? { opacity: 0 } : { opacity: 0, rotate: -90, scale: 0.5 }}
        animate={{ opacity: 1, rotate: 0, scale: 1 }}
        exit={reduced ? { opacity: 0 } : { opacity: 0, rotate: 90, scale: 0.5 }}
        transition={reduced ? { duration: 0 } : SPRING_IOS_FAST}>
        <Icon size={18} strokeWidth={1.9} />
      </motion.span>
    </AnimatePresence>
  </button>;
}
