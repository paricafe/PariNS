import { useEffect, useRef, useState } from 'react';
import { flushSync } from 'react-dom';
import { chooseLanguage, type Language } from '../i18n';
import {
  DEFAULT_BACKGROUND, STORAGE_KEYS, clampOverlay, readPreference, saveBackground, saveOverlay, saveTheme,
  storedBackground, storedOverlay, storedTheme, writePreference, type BackgroundId, type Theme,
} from './preferences';
import { clearImage, loadImage, saveImage } from './imageStore';
import { prepareImage } from './image';

export interface CustomImage { url: string; luminance: number }

const prefersDark = () => typeof matchMedia === 'function' && matchMedia('(prefers-color-scheme: dark)').matches;
const prefersReducedMotion = () => typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;
const resolveDark = (theme: Theme, systemDark: boolean) => theme === 'dark' || theme === 'system' && systemDark;

interface ViewTransition { ready: Promise<void>; finished: Promise<void> }

function withThemeTransition(update: () => void) {
  const start = (document as Document & { startViewTransition?: (callback: () => void) => ViewTransition }).startViewTransition;
  if (typeof start !== 'function' || prefersReducedMotion() || document.visibilityState !== 'visible') { update(); return; }
  const transition = start.call(document, () => flushSync(update));
  transition.ready.catch(() => {});
  transition.finished.catch(() => {});
}

export function useAppearance() {
  const [language, setLanguage] = useState<Language>(() => chooseLanguage(readPreference(STORAGE_KEYS.language), navigator.languages));
  const [theme, setTheme] = useState<Theme>(storedTheme);
  const [systemDark, setSystemDark] = useState(prefersDark);
  const [background, setBackground] = useState<BackgroundId>(storedBackground);
  const [overlay, setOverlay] = useState(storedOverlay);
  const [image, setImage] = useState<CustomImage | null>(null);
  const imageUrl = useRef<string | null>(null);
  const dark = resolveDark(theme, systemDark);

  useEffect(() => {
    if (typeof matchMedia !== 'function') return;
    const media = matchMedia('(prefers-color-scheme: dark)');
    const update = () => setSystemDark(media.matches);
    media.addEventListener('change', update);
    return () => media.removeEventListener('change', update);
  }, []);
  useEffect(() => { document.documentElement.classList.toggle('dark', dark); }, [dark]);
  useEffect(() => { document.documentElement.lang = language; }, [language]);
  useEffect(() => { document.documentElement.dataset.bg = background; }, [background]);

  const showImage = (blob: Blob, luminance: number) => {
    if (imageUrl.current) URL.revokeObjectURL(imageUrl.current);
    imageUrl.current = URL.createObjectURL(blob);
    setImage({ url: imageUrl.current, luminance });
  };
  useEffect(() => {
    let active = true;
    if (storedBackground() === 'custom') void loadImage().then((stored) => {
      if (!active) return;
      if (stored) showImage(stored.blob, stored.luminance);
      else { setBackground(DEFAULT_BACKGROUND); saveBackground(DEFAULT_BACKGROUND); }
    });
    return () => { active = false; if (imageUrl.current) URL.revokeObjectURL(imageUrl.current); imageUrl.current = null; };
  }, []);

  const changeLanguage = (next: Language) => { setLanguage(next); writePreference(STORAGE_KEYS.language, next); };
  const changeTheme = (next: Theme) => {
    saveTheme(next);
    withThemeTransition(() => {
      document.documentElement.classList.toggle('dark', resolveDark(next, prefersDark()));
      setTheme(next);
    });
  };
  const changeBackground = (next: BackgroundId) => { setBackground(next); saveBackground(next); };
  const changeOverlay = (next: number) => { const value = clampOverlay(next); setOverlay(value); saveOverlay(value); };
  const uploadImage = async (file: Blob): Promise<{ saved: boolean }> => {
    const prepared = await prepareImage(file);
    const saved = await saveImage({ blob: prepared.blob, luminance: prepared.luminance });
    showImage(prepared.blob, prepared.luminance);
    setBackground('custom');
    if (saved) saveBackground('custom');
    return { saved };
  };
  const removeImage = async () => {
    await clearImage();
    if (imageUrl.current) URL.revokeObjectURL(imageUrl.current);
    imageUrl.current = null;
    setImage(null);
    if (background === 'custom') { setBackground(DEFAULT_BACKGROUND); saveBackground(DEFAULT_BACKGROUND); }
  };

  return { language, theme, dark, background, overlay, image, changeLanguage, changeTheme, changeBackground, changeOverlay, uploadImage, removeImage };
}

export type Appearance = ReturnType<typeof useAppearance>;
