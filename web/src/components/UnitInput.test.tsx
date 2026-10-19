// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { UnitInput, unitFamily } from './UnitInput';

afterEach(cleanup);

describe('UnitInput', () => {
  it('shows the largest exact unit and submits the Rust base unit', () => {
    const onChange = vi.fn();
    const view = render(<UnitInput id="size" label="Size" family="bytes" value="33554432" language="en" onChange={onChange} />);
    const input = screen.getByRole('textbox') as HTMLInputElement;
    const unit = () => screen.getByRole('radiogroup', { name: 'Size · Unit' }).querySelector('[aria-checked="true"]')?.textContent;
    expect(input.value).toBe('32');
    expect(unit()).toBe('MB');
    fireEvent.change(input, { target: { value: '64' } });
    expect(onChange).toHaveBeenLastCalledWith('67108864');
    fireEvent.click(screen.getByRole('radio', { name: 'GB' }));
    expect(onChange).toHaveBeenLastCalledWith(String(64 * 1024 ** 3));
    // An external reload re-derives the display; an own edit does not.
    view.rerender(<UnitInput id="size" label="Size" family="bytes" value="2048" language="en" onChange={onChange} />);
    expect(input.value).toBe('2');
    expect(unit()).toBe('KB');
    // A value that is not a whole number of a larger unit stays in bytes.
    view.rerender(<UnitInput id="size" label="Size" family="bytes" value="1536" language="en" onChange={onChange} />);
    expect(input.value).toBe('1536');
  });

  it('keeps incomplete or inexact input as a raw draft for validation', () => {
    const onChange = vi.fn();
    render(<UnitInput id="timeout" label="Timeout" family="milliseconds" value="1000" language="en" onChange={onChange} />);
    const input = screen.getByRole('textbox') as HTMLInputElement;
    expect(input.value).toBe('1');
    fireEvent.change(input, { target: { value: '2.' } });
    expect(onChange).toHaveBeenLastCalledWith('2.');
    fireEvent.change(input, { target: { value: '' } });
    expect(onChange).toHaveBeenLastCalledWith('');
    fireEvent.change(input, { target: { value: '0.0001' } });
    expect(onChange).toHaveBeenLastCalledWith('0.0001');
    fireEvent.change(input, { target: { value: '1.5' } });
    expect(onChange).toHaveBeenLastCalledWith('1500');
    // Exact decimal conversion: 1.001 * 1000 is 1000.9999999999999 in floating point, but 1.001 s is exactly 1001 ms.
    fireEvent.change(input, { target: { value: '1.001' } });
    expect(onChange).toHaveBeenLastCalledWith('1001');
  });

  it('offers only units whose one unit fits under the maximum, keeping the unit in use', () => {
    const units = () => screen.getAllByRole('radio').map((radio) => radio.textContent);
    const view = render(<UnitInput id="snapshot" label="Snapshot" family="bytes" max={512 * 1024 ** 2} value={String(32 * 1024 ** 2)} language="en" onChange={() => {}} />);
    expect(units()).toEqual(['B', 'KB', 'MB']);
    // An out-of-range legacy value already in GB keeps its unit so the display stays correct.
    view.rerender(<UnitInput id="snapshot" label="Snapshot" family="bytes" max={512 * 1024 ** 2} value={String(2 * 1024 ** 3)} language="en" onChange={() => {}} />);
    expect(units()).toEqual(['B', 'KB', 'MB', 'GB']);
    cleanup();
    render(<UnitInput id="ttl" label="TTL" family="seconds" max={3600} value="60" language="en" onChange={() => {}} />);
    expect(units()).toEqual(['seconds', 'minutes', 'hours']);
  });

  it('only applies to byte, millisecond and second fields', () => {
    expect(unitFamily('cache.max_bytes')).toBe('bytes');
    expect(unitFamily('query_timeout_ms')).toBe('milliseconds');
    expect(unitFamily('cache.max_ttl_secs')).toBe('seconds');
    expect(unitFamily('cache.max_entries')).toBeNull();
  });
});
