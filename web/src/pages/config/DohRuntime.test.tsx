// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, render, screen, waitFor } from '@testing-library/react';
import type { ApiClient } from '../../session/client';
import { DohRuntime } from './DohRuntime';

afterEach(cleanup);
describe('DoH runtime summary', () => {
  it('uses the actual assigned IPv6 port for TCP and UDP, with bilingual labels', async () => {
    const api = { request: vi.fn(async () => ({ running: true, doh_listen: '[::1]:45678', doh_http3: true })) } as unknown as ApiClient;
    const view = render(<DohRuntime api={api} language="en" />);
    await waitFor(() => expect(screen.getByText('[::1]:45678 · /dns-query')).toBeTruthy());
    expect(screen.getByText('HTTP/2 · TCP 45678 / HTTP/3 · UDP 45678')).toBeTruthy();
    view.rerender(<DohRuntime api={api} language="zh-CN" />);
    expect(screen.getByRole('heading', { name: '正在运行的 DoH' })).toBeTruthy();
    expect(api.request).toHaveBeenCalledTimes(1);
  });
  it('never presents a stopped DNS listener as running', async () => {
    const api = { request: vi.fn(async () => ({ running: false, doh_listen: null, doh_http3: false })) } as unknown as ApiClient;
    render(<DohRuntime api={api} language="en" />);
    await waitFor(() => expect(screen.getByText(/DoH is not running/)).toBeTruthy());
    expect(screen.queryByText(/HTTP\/2 · TCP/)).toBeNull();
  });
});
