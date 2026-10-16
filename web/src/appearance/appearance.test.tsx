// @vitest-environment jsdom
import { useState } from 'react';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ThemeButton } from '../components/ThemeButton';
import { Backdrop } from '../components/Backdrop';
import { useAppearance, type Appearance } from './useAppearance';
import {
  STORAGE_KEYS, effectiveOverlay, nextTheme, saveBackground, saveOverlay, storedBackground, storedOverlay, storedTheme, writePreference,
} from './preferences';
import { clearImage, loadImage, saveImage } from './imageStore';
import { ImageError, MAX_BYTES, fitWithin, headerSize, prepareImage } from './image';

/** Minimal in-memory IndexedDB: one database, object stores with get/put/delete. */
function installFakeIndexedDB() {
  const stores = new Map<string, Map<string, unknown>>();
  const later = (callback: () => void) => setTimeout(callback, 0);
  const request = <T,>(compute: () => T) => {
    const result: { result?: T; error: null; onsuccess?: () => void } = { error: null };
    result.result = compute();
    return result;
  };
  const database = {
    createObjectStore: (name: string) => { stores.set(name, new Map()); },
    close: () => {},
    transaction: (name: string) => {
      const transaction: { oncomplete?: () => void; onerror?: () => void; onabort?: () => void; error: null; objectStore(): unknown } = {
        error: null,
        objectStore: () => {
          const store = stores.get(name)!;
          return {
            get: (key: string) => request(() => store.get(key)),
            put: (value: unknown, key: string) => request(() => { store.set(key, value); return key; }),
            delete: (key: string) => request(() => { store.delete(key); return undefined; }),
          };
        },
      };
      later(() => transaction.oncomplete?.());
      return transaction;
    },
  };
  const fake = {
    open: () => {
      const open: { result: typeof database; onupgradeneeded?: () => void; onsuccess?: () => void } = { result: database };
      later(() => { if (!stores.size) open.onupgradeneeded?.(); open.onsuccess?.(); });
      return open;
    },
  };
  vi.stubGlobal('indexedDB', fake);
  return stores;
}

let appearance: Appearance;
function Harness() {
  appearance = useAppearance();
  const [draft, setDraft] = useState('');
  return <>
    <Backdrop background={appearance.background} image={appearance.image} overlay={appearance.overlay} dark={appearance.dark} />
    <label>Draft<input value={draft} onChange={(event) => setDraft(event.target.value)} /></label>
    <ThemeButton theme={appearance.theme} language={appearance.language} onChange={appearance.changeTheme} />
  </>;
}

beforeEach(() => {
  localStorage.clear();
  document.documentElement.className = '';
  delete document.documentElement.dataset.bg;
  vi.stubGlobal('URL', Object.assign(URL, { createObjectURL: vi.fn(() => 'blob:background'), revokeObjectURL: vi.fn() }));
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

describe('theme button', () => {
  it('cycles system → light → dark with state labels, persisting only the theme and keeping drafts and focus', () => {
    render(<Harness />);
    const draft = screen.getByRole('textbox', { name: 'Draft' }) as HTMLInputElement;
    fireEvent.change(draft, { target: { value: 'unsaved edit' } });
    const button = () => screen.getByRole('button', { name: /^(Theme|主题)/ });
    expect(button().getAttribute('aria-label')).toMatch(/跟随系统|System/);
    act(() => appearance.changeLanguage('en'));
    expect(button().getAttribute('aria-label')).toBe('Theme: System');
    expect(button().getAttribute('title')).toBe('Theme: System (select for Light)');
    button().focus();
    fireEvent.click(button());
    expect(button().getAttribute('aria-label')).toBe('Theme: Light');
    expect(localStorage.getItem(STORAGE_KEYS.theme)).toBe('light');
    expect(document.documentElement.classList.contains('dark')).toBe(false);
    fireEvent.click(button());
    expect(button().getAttribute('aria-label')).toBe('Theme: Dark');
    expect(localStorage.getItem(STORAGE_KEYS.theme)).toBe('dark');
    expect(document.documentElement.classList.contains('dark')).toBe(true);
    fireEvent.click(button());
    expect(button().getAttribute('aria-label')).toBe('Theme: System');
    expect(localStorage.getItem(STORAGE_KEYS.theme)).toBeNull();
    expect(document.activeElement).toBe(button());
    expect(draft.value).toBe('unsaved edit');
    expect(Object.keys(localStorage).sort()).toEqual([STORAGE_KEYS.language]);
    expect(nextTheme('dark')).toBe('system');
  });

  it('works with defaults when storage throws', () => {
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => { throw new Error('denied'); });
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('denied'); });
    vi.spyOn(Storage.prototype, 'removeItem').mockImplementation(() => { throw new Error('denied'); });
    expect(storedTheme()).toBe('system');
    expect(storedBackground()).toBe('neutral');
    expect(storedOverlay()).toBe(40);
    expect(writePreference(STORAGE_KEYS.theme, 'dark')).toBe(false);
    render(<Harness />);
    fireEvent.click(screen.getByRole('button', { name: /^(Theme|主题)/ }));
    expect(appearance.theme).toBe('light');
  });
});

describe('background preferences', () => {
  it('stores non-default choices, removes defaults and rejects unknown values', () => {
    saveBackground('mist'); saveOverlay(65);
    expect(storedBackground()).toBe('mist');
    expect(storedOverlay()).toBe(65);
    saveBackground('neutral'); saveOverlay(40);
    expect(localStorage.getItem(STORAGE_KEYS.background)).toBeNull();
    expect(localStorage.getItem(STORAGE_KEYS.overlay)).toBeNull();
    localStorage.setItem(STORAGE_KEYS.background, 'javascript:alert(1)');
    localStorage.setItem(STORAGE_KEYS.overlay, '400');
    expect(storedBackground()).toBe('neutral');
    expect(storedOverlay()).toBe(90);
  });

  it('applies the choice to the root and to its own backdrop layer', () => {
    render(<Harness />);
    act(() => appearance.changeBackground('grid'));
    expect(document.documentElement.dataset.bg).toBe('grid');
    expect(document.documentElement.dataset.backdrop).toBe('pattern');
    expect(document.querySelector('.backdrop-layer[data-bg="grid"] .grid')).toBeTruthy();
    expect(localStorage.getItem(STORAGE_KEYS.background)).toBe('grid');
    // Without an image the custom choice is refused.
    act(() => appearance.changeBackground('custom'));
    expect(appearance.background).toBe('grid');
  });

  it('strengthens the text backing when the image fights the theme', () => {
    expect(effectiveOverlay(10, 0.9, true)).toBe(55);
    expect(effectiveOverlay(10, 0.9, false)).toBe(20);
    expect(effectiveOverlay(70, 0.1, false)).toBe(70);
    expect(effectiveOverlay(10, null, true)).toBe(10);
  });
});

describe('custom image storage', () => {
  it('tolerates a browser without IndexedDB', async () => {
    vi.stubGlobal('indexedDB', undefined);
    await expect(loadImage()).resolves.toBeNull();
    await expect(saveImage({ blob: new Blob(['x']), luminance: 0.5 })).resolves.toBe(false);
    await expect(clearImage()).resolves.toBe(false);
  });

  it('restores a stored image, then clears it back to the default background', async () => {
    const stores = installFakeIndexedDB();
    await saveImage({ blob: new Blob(['pixels'], { type: 'image/webp' }), luminance: 0.8 });
    localStorage.setItem(STORAGE_KEYS.background, 'custom');
    render(<Harness />);
    await waitFor(() => expect(appearance.image?.url).toBe('blob:background'));
    expect(appearance.background).toBe('custom');
    // The backdrop kind is published by an effect after the render that loads the
    // image, so wait for it rather than asserting in the same tick.
    await waitFor(() => expect(document.documentElement.dataset.backdrop).toBe('image'));
    expect(document.querySelector('.backdrop .photo')).toBeTruthy();
    await act(() => appearance.removeImage());
    expect(appearance.image).toBeNull();
    expect(appearance.background).toBe('neutral');
    expect(localStorage.getItem(STORAGE_KEYS.background)).toBeNull();
    expect(stores.get('background')?.size).toBe(0);
  });

  it('falls back to the default when the stored image is gone', async () => {
    installFakeIndexedDB();
    localStorage.setItem(STORAGE_KEYS.background, 'custom');
    render(<Harness />);
    await waitFor(() => expect(appearance.background).toBe('neutral'));
    expect(localStorage.getItem(STORAGE_KEYS.background)).toBeNull();
  });
});

describe('image preparation', () => {
  function mockCanvas(sizes: (type: string, quality: number) => number) {
    const drawn: Array<[number, number]> = [];
    const encoded: Array<[string, number]> = [];
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockImplementation(function (this: HTMLCanvasElement) {
      const canvas = this;
      return { drawImage: () => drawn.push([canvas.width, canvas.height]), getImageData: () => ({ data: new Uint8ClampedArray(8 * 8 * 4).fill(255) }) } as unknown as CanvasRenderingContext2D;
    } as never);
    vi.spyOn(HTMLCanvasElement.prototype, 'toBlob').mockImplementation((callback, type = 'image/png', quality = 1) => {
      encoded.push([type, quality]);
      callback(new Blob([new Uint8Array(sizes(type, quality))], { type }));
    });
    return { drawn, encoded };
  }

  it('downscales to the long-edge limit and lowers quality until it fits the byte budget', async () => {
    const close = vi.fn();
    vi.stubGlobal('createImageBitmap', vi.fn(async () => ({ width: 5120, height: 2880, close })));
    const { drawn, encoded } = mockCanvas((_type, quality) => quality > 0.75 ? MAX_BYTES + 1 : 1000);
    const prepared = await prepareImage(new File(['x'], 'photo.jpg', { type: 'image/jpeg' }));
    expect([prepared.width, prepared.height]).toEqual([2560, 1440]);
    expect(drawn).toContainEqual([2560, 1440]);
    expect(encoded).toEqual([['image/webp', 0.86], ['image/webp', 0.78], ['image/webp', 0.7]]);
    expect(prepared.blob.type).toBe('image/webp');
    expect(prepared.luminance).toBeCloseTo(1);
    expect(close).toHaveBeenCalled();
  });

  it('re-encodes as JPEG without a WebP encoder and rejects unsupported files', async () => {
    vi.stubGlobal('createImageBitmap', vi.fn(async () => ({ width: 800, height: 600 })));
    // A browser without WebP encoding returns PNG for any requested type.
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue({ drawImage() {}, getImageData: () => { throw new Error('tainted'); } } as never);
    vi.spyOn(HTMLCanvasElement.prototype, 'toBlob').mockImplementation((callback, type) => callback(new Blob(['x'], { type: type === 'image/jpeg' ? 'image/jpeg' : 'image/png' })));
    const prepared = await prepareImage(new File(['x'], 'a.png', { type: 'image/png' }));
    expect(prepared.blob.type).toBe('image/jpeg');
    expect([prepared.width, prepared.height]).toEqual([800, 600]);
    expect(prepared.luminance).toBe(0.5);
    await expect(prepareImage(new File(['x'], 'a.gif', { type: 'image/gif' }))).rejects.toEqual(new ImageError('type'));
    expect(fitWithin(1000, 4000)).toEqual({ width: 640, height: 2560 });
  });

  it('reads image sizes from headers and refuses huge images before decoding', async () => {
    const png = (width: number, height: number) => {
      const bytes = new Uint8Array(33);
      bytes.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52]);
      new DataView(bytes.buffer).setUint32(16, width);
      new DataView(bytes.buffer).setUint32(20, height);
      return bytes;
    };
    expect(headerSize(png(5120, 2880))).toEqual({ width: 5120, height: 2880 });
    const jpeg = new Uint8Array([0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xc0, 0, 17, 8, 0x0b, 0xb8, 0x0f, 0xa0, 3]);
    expect(headerSize(jpeg)).toEqual({ width: 4000, height: 3000 });
    expect(headerSize(new Uint8Array([1, 2, 3]))).toBeNull();
    const decode = vi.fn(async () => ({ width: 1, height: 1 }));
    vi.stubGlobal('createImageBitmap', decode);
    // 20000 x 20000 = 400 megapixels in a tiny file: refused without decoding.
    await expect(prepareImage(new File([png(20000, 20000)], 'bomb.png', { type: 'image/png' }))).rejects.toEqual(new ImageError('size'));
    expect(decode).not.toHaveBeenCalled();
  });
});
