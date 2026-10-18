// Browser-local appearance preferences. Only language, theme, background choice and
// overlay strength live in localStorage (the custom image itself lives in IndexedDB,
// see imageStore.ts). Never store credentials, session bindings, PEM, logs or drafts here.
// Every access tolerates unavailable or throwing storage: the console then runs with defaults.

export type Theme = 'system' | 'light' | 'dark';
export const THEME_ORDER: readonly Theme[] = ['system', 'light', 'dark'];
export const nextTheme = (theme: Theme): Theme => THEME_ORDER[(THEME_ORDER.indexOf(theme) + 1) % THEME_ORDER.length];

/** Solid presets (light and dark values live in style.css under [data-bg]), patterns, and the uploaded image. */
export const SOLID_BACKGROUNDS = ['neutral', 'mist', 'sand', 'sage', 'lavender', 'slate'] as const;
export const PATTERN_BACKGROUNDS = ['orbs', 'grid', 'gradient'] as const;
export type BackgroundId = typeof SOLID_BACKGROUNDS[number] | typeof PATTERN_BACKGROUNDS[number] | 'custom';
export const DEFAULT_BACKGROUND: BackgroundId = 'neutral';
export const DEFAULT_OVERLAY = 40;
export const MAX_OVERLAY = 90;

export const STORAGE_KEYS = {
  language: 'parins-language',
  theme: 'parins-theme',
  background: 'parins-background',
  overlay: 'parins-background-overlay',
} as const;

const backgrounds: readonly string[] = [...SOLID_BACKGROUNDS, ...PATTERN_BACKGROUNDS, 'custom'];
export const isBackground = (value: unknown): value is BackgroundId => typeof value === 'string' && backgrounds.includes(value);
export const isPattern = (value: BackgroundId) => (PATTERN_BACKGROUNDS as readonly string[]).includes(value);

export function readPreference(key: string): string | null {
  try { return localStorage.getItem(key); } catch { return null; }
}

/** Writes or removes (null) a preference; returns false when the browser refuses storage. */
export function writePreference(key: string, value: string | null): boolean {
  try {
    if (value === null) localStorage.removeItem(key); else localStorage.setItem(key, value);
    return true;
  } catch { return false; }
}

export function storedTheme(): Theme {
  const stored = readPreference(STORAGE_KEYS.theme);
  return stored === 'light' || stored === 'dark' ? stored : 'system';
}
export const saveTheme = (theme: Theme) => writePreference(STORAGE_KEYS.theme, theme === 'system' ? null : theme);

export function storedBackground(): BackgroundId {
  const stored = readPreference(STORAGE_KEYS.background);
  return isBackground(stored) ? stored : DEFAULT_BACKGROUND;
}
export const saveBackground = (background: BackgroundId) => writePreference(STORAGE_KEYS.background, background === DEFAULT_BACKGROUND ? null : background);

export const clampOverlay = (value: number) => Math.min(MAX_OVERLAY, Math.max(0, Math.round(value)));
export function storedOverlay(): number {
  const stored = Number(readPreference(STORAGE_KEYS.overlay) ?? NaN);
  return Number.isFinite(stored) ? clampOverlay(stored) : DEFAULT_OVERLAY;
}
export const saveOverlay = (overlay: number) => writePreference(STORAGE_KEYS.overlay, overlay === DEFAULT_OVERLAY ? null : String(clampOverlay(overlay)));

/**
 * Text backing for a custom image: a bright image under the dark theme (or a dark image under
 * the light theme) needs a stronger canvas-coloured scrim so glass text keeps WCAG AA contrast.
 */
export function effectiveOverlay(overlay: number, luminance: number, dark: boolean): number {
  const mismatch = dark ? luminance > 0.45 : luminance < 0.4;
  return Math.max(overlay, mismatch ? 55 : 20);
}
