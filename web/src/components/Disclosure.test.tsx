// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { Disclosure } from './Disclosure';

afterEach(cleanup);

describe('Disclosure', () => {
  it('keeps collapsed content out of the focus order and the accessibility tree', () => {
    render(<Disclosure title="Advanced"><button type="button">Inner</button></Disclosure>);
    const toggle = screen.getByRole('button', { name: 'Advanced' });
    const region = document.getElementById(toggle.getAttribute('aria-controls')!)!;
    expect(region.hasAttribute('inert')).toBe(true);
    fireEvent.click(toggle);
    expect(region.hasAttribute('inert')).toBe(false);
  });
});
