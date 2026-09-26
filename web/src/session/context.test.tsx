// @vitest-environment jsdom
import { act, renderHook, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { SessionProvider, useSession } from './context';
const updateReload = vi.hoisted(() => vi.fn());
vi.mock('../features/updates/reload', () => ({ reloadConsole: updateReload }));

const session = { setup_required: false, authenticated: true, session: { binding: 'binding-1', expires_in_seconds: 28000 },
  transport: { scheme: 'http', origin: null, certificate_source: null } };
const jsonResponse = (value: unknown) => ({ ok: true, status: 200, json: async () => value }) as Response;

afterEach(() => { vi.unstubAllGlobals(); updateReload.mockClear(); });

describe('session transition', () => {
  it('reloads the embedded console only when an armed update reconnects to an expired session', async () => {
    vi.stubGlobal('fetch', vi.fn(async (url: string) => url === '/api/session' ? jsonResponse(session)
      : ({ ok: false, status: 401, json: async () => ({ error: { code: 'UNAUTHORIZED', message: 'Sign in' } }) }) as Response));
    const hook = renderHook(() => useSession(), { wrapper: SessionProvider });
    await waitFor(() => expect(hook.result.current.state.phase).toBe('ready'));
    act(() => hook.result.current.expectUpdateRestart(true));
    await act(async () => { await hook.result.current.api.request('updates').catch(() => {}); });
    expect(updateReload).toHaveBeenCalledTimes(1);
    expect(hook.result.current.api.currentBinding).toBeNull();
    hook.unmount();
  });

  it('explicit logout clears update restart intent instead of reloading', async () => {
    vi.stubGlobal('fetch', vi.fn(async (url: string) => jsonResponse(url === '/api/session' ? session : {})));
    const hook = renderHook(() => useSession(), { wrapper: SessionProvider });
    await waitFor(() => expect(hook.result.current.state.phase).toBe('ready'));
    act(() => hook.result.current.expectUpdateRestart(true));
    await act(async () => { await hook.result.current.logout(); });
    expect(updateReload).not.toHaveBeenCalled();
    expect(hook.result.current.state.phase).toBe('login');
    hook.unmount();
  });
  it('updates the security transport source on same-origin session refresh', async () => {
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url !== '/api/session') throw new Error(`Unexpected request ${url}`);
      return jsonResponse({ ...session, transport: {
        scheme: 'https', origin: 'https://dns.example.com:3000', certificate_source: 'doh',
      } });
    }));
    const hook = renderHook(() => useSession(), { wrapper: SessionProvider });
    await waitFor(() => expect(hook.result.current.state.transport?.certificate_source).toBe('doh'));
    await act(async () => { await hook.result.current.recheck(); });
    expect(hook.result.current.state.phase).toBe('ready');
    expect(hook.result.current.state.transport?.certificate_source).toBe('doh');
    hook.unmount();
  });

  it('logs in without Web Locks and confirms the browser Cookie session', async () => {
    let checks = 0;
    const fetcher = vi.fn(async (url: string) => {
      if (url === '/api/session') return jsonResponse(checks++ < 2
        ? { ...session, authenticated: false, session: null }
        : session);
      if (url === '/api/login') return jsonResponse(session);
      throw new Error(`Unexpected request ${url}`);
    });
    vi.stubGlobal('fetch', fetcher);
    const hook = renderHook(() => useSession(), { wrapper: SessionProvider });
    await waitFor(() => expect(hook.result.current.state.phase).toBe('login'));
    await act(async () => { await hook.result.current.login('admin', 'password'); });
    expect(hook.result.current.state.phase).toBe('ready');
    expect(checks).toBe(3); // bootstrap, preflight, Cookie confirmation
    hook.unmount();
  });

  it('does not confirm an HTTP setup Cookie after setup switches to HTTPS', async () => {
    let checks = 0;
    const change = { from: 'http', to: 'https', next_origin: null, reauthenticate: true, requires_http_confirmation: false };
    const fetcher = vi.fn(async (url: string) => {
      if (url === '/api/session') { checks += 1; return jsonResponse({ ...session, setup_required: true, authenticated: false, session: null }); }
      if (url === '/api/setup/preview') return jsonResponse({ valid: true, transport_change: change });
      if (url === '/api/setup') return jsonResponse({ ...session, authenticated: false, session: null, transport_change: change });
      throw new Error(`Unexpected request ${url}`);
    });
    vi.stubGlobal('fetch', fetcher);
    const hook = renderHook(() => useSession(), { wrapper: SessionProvider });
    await waitFor(() => expect(hook.result.current.state.phase).toBe('setup'));
    await act(async () => { await hook.result.current.setup('admin', 'password', 'toml', 'token'); });
    expect(hook.result.current.state.phase).toBe('transport-change');
    expect(checks).toBe(2); // bootstrap and preflight, never old-HTTP confirmLogin
    hook.unmount();
  });

  it('masks a setup whose response was lost and never resubmits it', async () => {
    const nextOrigin = 'https://dns.example.com:3000';
    let oldOriginFailed = false;
    const fetcher = vi.fn(async (url: string) => {
      if (url === '/api/session') {
        if (oldOriginFailed) throw new TypeError('old HTTP origin closed');
        return jsonResponse({ ...session, setup_required: true, authenticated: false, session: null });
      }
      if (url === '/api/setup/preview') return jsonResponse({ valid: true, transport_change: {
        from: 'http', to: 'https', next_origin: nextOrigin, reauthenticate: true, requires_http_confirmation: false,
      } });
      if (url === '/api/setup') throw new TypeError('connection closed');
      throw new Error(`Unexpected request ${url}`);
    });
    vi.stubGlobal('fetch', fetcher);
    const hook = renderHook(() => useSession(), { wrapper: SessionProvider });
    await waitFor(() => expect(hook.result.current.state.phase).toBe('setup'));
    await act(async () => { await expect(hook.result.current.setup('admin', 'password', 'toml', 'token')).rejects.toMatchObject({ code: 'NETWORK' }); });
    expect(hook.result.current.state.phase).toBe('setup-unknown');
    expect(hook.result.current.state.nextOrigin).toBe(nextOrigin);
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/setup')).toHaveLength(1);
    oldOriginFailed = true;
    await act(async () => { await hook.result.current.recheck(); });
    expect(hook.result.current.state.phase).toBe('setup-unknown');
    expect(hook.result.current.state.nextOrigin).toBe(nextOrigin);
    hook.unmount();
  });

  it('keeps private UI masked when a pre-logout session check finishes late', async () => {
    let finishOldCheck!: (response: Response) => void;
    const oldCheck = new Promise<Response>((resolve) => { finishOldCheck = resolve; });
    let finishLogout!: (response: Response) => void;
    const logoutResponse = new Promise<Response>((resolve) => { finishLogout = resolve; });
    let checks = 0;
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
    await act(async () => { finishLogout(jsonResponse({ setup_required: false, authenticated: false, session: null, transport: session.transport })); await logout; });
    expect(hook.result.current.state.phase).toBe('login');
  });
});
