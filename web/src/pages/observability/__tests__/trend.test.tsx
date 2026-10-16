// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { TrendChart } from "../TrendChart";
import type { HistorySample } from "../metrics";

afterEach(() => { cleanup(); vi.restoreAllMocks(); });

const now = Date.UTC(2026, 9, 16, 12, 0);
const minute = 60_000;
const sample = (minutesAgo: number, requests: number, generation = 1, running = true): HistorySample => ({
  timestamp_ms: now - minutesAgo * minute, elapsed_seconds: 60, running, generation, requests, cache_hits: requests / 2, blocked: requests / 4,
});
// 1/s, 2/s, a stopped sample, then a new run at 3/s and 4/s.
const samples = [sample(50, 60), sample(49, 120), sample(48, 0, 1, false), sample(30, 180, 2), sample(29, 240, 2)];
// Default width 770 with no ResizeObserver: plot spans x = 58..754 over one hour.
const xFor = (minutesAgo: number) => 58 + (60 - minutesAgo) / 60 * 696;

function mount() {
  vi.spyOn(SVGElement.prototype, 'getBoundingClientRect').mockReturnValue({ left: 0, top: 0, width: 770, height: 244, right: 770, bottom: 244, x: 0, y: 0, toJSON() {} } as DOMRect);
  render(<TrendChart samples={samples} hours={1} end={now} language="en" />);
  return { chart: screen.getByRole('group'), svg: screen.getByRole('img') };
}
const tip = () => screen.queryByRole('tooltip');

describe('TrendChart hover', () => {
  it('snaps to the nearest sample with a reference line and per-second values', () => {
    const { svg } = mount();
    fireEvent.pointerMove(svg, { clientX: xFor(49) - 4, pointerType: 'mouse' });
    expect(tip()?.textContent).toMatch(/Queries\/s\s*2/);
    expect(tip()?.textContent).toMatch(/Cache hits\/s\s*1/);
    expect(tip()?.textContent).toMatch(/Blocked\/s\s*0\.5/);
    const cursor = document.querySelector('.trend-cursor line')!;
    expect(Number(cursor.getAttribute('x1'))).toBeCloseTo(xFor(49));
    expect(document.querySelectorAll('.trend-cursor circle')).toHaveLength(3);
    fireEvent.pointerLeave(svg, { pointerType: 'mouse' });
    expect(tip()).toBeNull();
  });

  it('explains a span where the service was not running', () => {
    const { svg } = mount();
    fireEvent.pointerMove(svg, { clientX: xFor(40), pointerType: 'mouse' });
    expect(tip()?.textContent).toContain("The service wasn't running during this time");
    expect(document.querySelector('.trend-gap[data-active]')).toBeTruthy();
    expect(document.querySelector('.trend-cursor')).toBeNull();
  });

  it('shows on tap without following touch moves', () => {
    const { svg } = mount();
    fireEvent.pointerMove(svg, { clientX: xFor(29), pointerType: 'touch' });
    expect(tip()).toBeNull();
    fireEvent.pointerDown(svg, { clientX: xFor(29), pointerType: 'touch' });
    expect(tip()?.textContent).toMatch(/Queries\/s\s*4/);
  });

  it('moves between points with the arrow keys and hides on Escape', () => {
    const { chart } = mount();
    expect(chart.tabIndex).toBe(0);
    fireEvent.keyDown(chart, { key: 'ArrowLeft' });
    expect(tip()?.textContent).toMatch(/Queries\/s\s*4/);
    expect(chart.getAttribute('aria-describedby')).toContain(tip()!.id);
    fireEvent.keyDown(chart, { key: 'ArrowLeft' });
    expect(tip()?.textContent).toMatch(/Queries\/s\s*3/);
    fireEvent.keyDown(chart, { key: 'ArrowLeft' });
    expect(tip()?.textContent).toContain("wasn't running");
    fireEvent.keyDown(chart, { key: 'ArrowLeft' });
    expect(tip()?.textContent).toMatch(/Queries\/s\s*2/);
    fireEvent.keyDown(chart, { key: 'ArrowRight' });
    expect(tip()?.textContent).toContain("wasn't running");
    fireEvent.keyDown(chart, { key: 'Escape' });
    expect(tip()).toBeNull();
  });
});
