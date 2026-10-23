// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { Select } from './Popover';

afterEach(() => { cleanup(); vi.restoreAllMocks(); });

/** jsdom has no layout: place the control and size the menu and viewport. */
function layout(control: { left: number; right: number }, menuWidth: number, viewport: number) {
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (this: HTMLElement) {
    return { ...control, top: 0, bottom: 32, width: control.right - control.left, height: 32, x: control.left, y: 0, toJSON: () => ({}) } as DOMRect;
  });
  vi.spyOn(HTMLElement.prototype, 'offsetWidth', 'get').mockReturnValue(menuWidth);
  vi.spyOn(document.documentElement, 'clientWidth', 'get').mockReturnValue(viewport);
}

function open() {
  render(<Select label="Mode" value="a" options={[{ value: 'a', label: 'A' }, { value: 'b', label: 'B' }]} onChange={() => {}} />);
  fireEvent.click(screen.getByRole('button', { name: 'Mode: A' }));
  return screen.getByRole('listbox');
}

describe('Select menu placement', () => {
  it('opens toward the right when it fits', () => {
    layout({ left: 100, right: 220 }, 200, 800);
    const menu = open();
    expect(menu.dataset.align).toBe('start');
    expect(menu.style.left).toBe('0px');
  });

  it('flips to end at the control’s right edge near the window edge', () => {
    layout({ left: 700, right: 760 }, 200, 800);
    expect(open().dataset.align).toBe('end');
  });

  it('shifts back inside a narrow window when neither side fits', () => {
    // 375 px: from 100 a 320 px menu passes the right edge (367) and would start
    // at -180 aligned to the control's right edge.
    layout({ left: 100, right: 140 }, 320, 375);
    const menu = open();
    expect(menu.dataset.align).toBe('start');
    expect(menu.style.left).toBe('-53px');
  });
});
