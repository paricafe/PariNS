// @vitest-environment jsdom
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { DohRuntime } from './DohRuntime';

afterEach(cleanup);
describe('DoH runtime summary', () => {
  it('uses the actual assigned IPv6 port for TCP and UDP, with bilingual labels', () => {
    const status = { running: true, doh_listen: '[::1]:45678', doh_http3: true };
    const view = render(<DohRuntime status={status} failed={false} language="en" />);
    expect(screen.getByText('[::1]:45678 · /dns-query')).toBeTruthy();
    expect(screen.getByText('HTTP/2 · TCP 45678 / HTTP/3 · UDP 45678')).toBeTruthy();
    view.rerender(<DohRuntime status={status} failed={false} language="zh-CN" />);
    expect(screen.getByRole('heading', { name: '正在运行的 DoH' })).toBeTruthy();
  });
  it('never presents a stopped DNS listener as running', () => {
    render(<DohRuntime status={{ running: false, doh_listen: null, doh_http3: false }} failed={false} language="en" />);
    expect(screen.getByText(/DoH is not running/)).toBeTruthy();
    expect(screen.queryByText(/HTTP\/2 · TCP/)).toBeNull();
  });
});
