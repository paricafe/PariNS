// @vitest-environment jsdom
import { act, renderHook, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { SessionProvider, useSession } from './context';

const session = { setup_required: false, authenticated: true, session: { binding: 'binding-1', expires_in_seconds: 28000 } };
const jsonResponse = (value: unknown) => ({ ok: true, status: 200, json: async () => value }) as Response;

afterEach(() => vi.unstubAllGlobals());

describe('session transition', () => {
  it('keeps private UI masked when a pre-logout session check finishes late', async () => {
    let finishOldCheck!: (response: Response) => void;
    const oldCheck = new Promise<Response>((resolve) => { finishOldCheck = resolve; });
    let finishLogout!: (response: Response) => void;
    const logoutResponse = new Promise<Response>((resolve) => { finishLogout = resolve; });
    let checks = 0;
    vi.stubGlobal('navigator', { locks: { request: async (_name: string, _options: unknown, callback: () => Promise<unknown>) => callback() } });
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url === '/api/session') return ++checks === 2 ? oldCheck : jsonResponse(session);
      if (url === '/api/logout') return logoutResponse;
      throw new Error(`Unexpected request ${url}`);
    }));
    const hook = renderHook(() => useSession(), { wrapper: SessionProvider });
    await waitFor(() => expect(hook.result.current.state.phase).toBe('ready'));
    act(() => { void hook.result.current.recheck(); });
    await waitFor(() => expect(checks).toBe(2));
    let logout!: Promise<void>;
    act(() => { logout = hook.result.current.logout(); });
    expect(hook.result.current.state.phase).toBe('logout-unknown');
    await act(async () => { finishOldCheck(jsonResponse(session)); await Promise.resolve(); });
    expect(hook.result.current.state.phase).toBe('logout-unknown');
    await act(async () => { finishLogout(jsonResponse({ setup_required: false, authenticated: false, session: null })); await logout; });
    expect(hook.result.current.state.phase).toBe('login');
  });
});
