// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { useAppearance } from '../appearance/useAppearance';
import { STORAGE_KEYS } from '../appearance/preferences';
import { AppearanceSheet } from './AppearanceSheet';
import { Backdrop } from './Backdrop';

beforeAll(() => {
  // Reduced motion (and a light system theme) for this file; motion reads matchMedia once per module.
  vi.stubGlobal('matchMedia', (query: string) => ({ matches: query.includes('reduced-motion'), media: query, addEventListener() {}, removeEventListener() {} }));
  HTMLDialogElement.prototype.showModal = function showModal() { this.setAttribute('open', ''); };
  HTMLDialogElement.prototype.close = function close() { this.removeAttribute('open'); };
});
beforeEach(() => { localStorage.clear(); localStorage.setItem(STORAGE_KEYS.language, 'en'); });
afterEach(cleanup);

function Harness() {
  const appearance = useAppearance();
  return <><Backdrop background={appearance.background} image={appearance.image} overlay={appearance.overlay} dark={appearance.dark} /><AppearanceSheet appearance={appearance} /></>;
}

describe('appearance sheet', () => {
  it('selects presets and patterns, keeps patterns static under reduced motion, and reports a refused upload', async () => {
    render(<Harness />);
    fireEvent.click(screen.getByRole('button', { name: 'Appearance' }));
    expect(screen.getByRole('dialog', { name: 'Appearance' })).toBeTruthy();
    fireEvent.click(screen.getByRole('radio', { name: 'Mist' }));
    expect(document.documentElement.dataset.bg).toBe('mist');
    expect(localStorage.getItem(STORAGE_KEYS.background)).toBe('mist');
    fireEvent.click(screen.getByRole('radio', { name: 'Drifting orbs' }));
    expect(document.querySelector('.backdrop')?.hasAttribute('data-static')).toBe(true);
    expect(document.querySelectorAll('.backdrop-layer[data-bg="orbs"] .orb')).toHaveLength(3);
    const input = document.querySelector('input[type=file]') as HTMLInputElement;
    await act(async () => { fireEvent.change(input, { target: { files: [new File(['x'], 'a.gif', { type: 'image/gif' })] } }); });
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe('Only PNG, JPEG or WebP images are supported.'));
    expect(screen.queryByRole('slider')).toBeNull();
  });
});
