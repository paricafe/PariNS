// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { InfoTip } from './InfoTip';

afterEach(cleanup);

describe('InfoTip', () => {
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
