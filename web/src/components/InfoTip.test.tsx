// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { InfoTip } from './InfoTip';

afterEach(() => { cleanup(); vi.restoreAllMocks(); });

describe('InfoTip', () => {
  it('keeps both edges inside the viewport and repositions after a resize', () => {
    const width = vi.spyOn(document.documentElement, 'clientWidth', 'get').mockReturnValue(390);
    vi.spyOn(HTMLElement.prototype, 'offsetWidth', 'get').mockReturnValue(300);
    render(<InfoTip text="Explanation" label="About" />);
    const anchor = screen.getByRole('button', { name: 'About' }).parentElement!;
    const bounds = vi.spyOn(anchor, 'getBoundingClientRect').mockReturnValue({ left: 121.375 } as DOMRect);
    fireEvent.click(screen.getByRole('button', { name: 'About' }));
    const note = screen.getByRole('note');
    expect(121.375 + parseFloat(note.style.left) + 300).toBeLessThanOrEqual(382);
    width.mockReturnValue(340);
    bounds.mockReturnValue({ left: 3 } as DOMRect);
    fireEvent(window, new Event('resize'));
    expect(3 + parseFloat(note.style.left)).toBeGreaterThanOrEqual(8);
    expect(3 + parseFloat(note.style.left) + 300).toBeLessThanOrEqual(332);
  });
  it('closes on Escape after focus moved away from its button', () => {
    render(<><InfoTip text="Explanation" label="About" /><input aria-label="Other" /></>);
    fireEvent.click(screen.getByRole('button', { name: 'About' }));
    expect(screen.getByRole('note').textContent).toBe('Explanation');
    const other = screen.getByRole('textbox', { name: 'Other' });
    other.focus();
    fireEvent.keyDown(other, { key: 'Escape' });
    expect(screen.getByRole('button', { name: 'About' }).getAttribute('aria-expanded')).toBe('false');
  });
});
