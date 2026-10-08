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
  it('rotates with the exact payload, clears only the local binding and requires an explicit new login', async () => {
    const messages: unknown[] = [];
    vi.stubGlobal('BroadcastChannel', class {
      onmessage = null;
      postMessage(value: unknown) { messages.push(value); }
      close() {}
    });
    let loggedIn = false;
    const fetcher = vi.fn(async (url: string) => {
      if (url === '/api/session') return jsonResponse(loggedIn ? { ...session, session: { ...session.session, binding: 'new-binding' } } : session);
      if (url === '/api/account/credentials') return jsonResponse({ reauthentication_required: true });
      if (url === '/api/login') { loggedIn = true; return jsonResponse({}); }
      throw new Error(`Unexpected request ${url}`);
    });
    vi.stubGlobal('fetch', fetcher);
    const hook = renderHook(() => useSession(), { wrapper: SessionProvider });
    await waitFor(() => expect(hook.result.current.state.phase).toBe('ready'));
    await act(async () => { await hook.result.current.rotateCredentials('current-password', 'new_admin', 'new-password-long'); });
    expect(hook.result.current.state).toMatchObject({ phase: 'login', credentials: 'changed' });
    expect(hook.result.current.api.currentBinding).toBeNull();
    const sent = (fetcher.mock.calls as unknown as [string, RequestInit][]).find(([url]) => url === '/api/account/credentials')![1];
    expect(JSON.parse(sent.body as string)).toEqual({ current_password: 'current-password', username: 'new_admin', new_password: 'new-password-long' });
    expect(sent.headers).toMatchObject({ 'X-PariNS-Session': 'binding-1' });
    expect(messages).toEqual(['changed']);
    const calls = fetcher.mock.calls.length;
    await act(async () => { await hook.result.current.recheck(); });
    expect(fetcher.mock.calls).toHaveLength(calls);
    await act(async () => { await hook.result.current.login('new_admin', 'new-password-long'); });
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/login')).toHaveLength(1);
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/logout')).toHaveLength(0);
    expect(hook.result.current.api.currentBinding).toBe('new-binding');
    expect(hook.result.current.state.phase).toBe('ready');
    hook.unmount();
  });

  it.each(['NETWORK', 'BAD_RESPONSE'])('does not replay a credential mutation with %s and checks entered credentials despite an old Cookie', async (code) => {
    const fetcher = vi.fn(async (url: string) => {
      if (url === '/api/session') return jsonResponse(session);
      if (url === '/api/account/credentials') {
        if (code === 'NETWORK') throw new TypeError('response lost');
        return jsonResponse({});
      }
      if (url === '/api/login') return ({ ok: false, status: 401, json: async () => ({ error: { code: 'LOGIN_FAILED', message: 'Not changed yet' } }) }) as Response;
      throw new Error(`Unexpected request ${url}`);
    });
    vi.stubGlobal('fetch', fetcher);
    const hook = renderHook(() => useSession(), { wrapper: SessionProvider });
    await waitFor(() => expect(hook.result.current.state.phase).toBe('ready'));
    await act(async () => { await expect(hook.result.current.rotateCredentials('current-password', 'new_admin', 'new-password-long')).rejects.toMatchObject({ code }); });
    expect(hook.result.current.state).toMatchObject({ phase: 'login', credentials: 'unknown' });
    expect(hook.result.current.api.currentBinding).toBeNull();
    await act(async () => { await hook.result.current.recheck(); });
    await act(async () => { await expect(hook.result.current.login('new_admin', 'new-password-long')).rejects.toMatchObject({ code: 'LOGIN_FAILED' }); });
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/account/credentials')).toHaveLength(1);
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/login')).toHaveLength(1);
    expect(hook.result.current.state).toMatchObject({ phase: 'login', credentials: 'unknown' });
    hook.unmount();
  });

  it('keeps a wrong-current-password 403 signed in while a revoked-session 401 follows normal session recovery', async () => {
    let revoked = false;
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url === '/api/session') return jsonResponse(revoked ? { ...session, authenticated: false, session: null } : session);
      return ({ ok: false, status: revoked ? 401 : 403, json: async () => ({ error: { code: revoked ? 'UNAUTHORIZED' : 'CURRENT_PASSWORD_INCORRECT', message: 'Rejected' } }) }) as Response;
    }));
    const hook = renderHook(() => useSession(), { wrapper: SessionProvider });
    await waitFor(() => expect(hook.result.current.state.phase).toBe('ready'));
    await act(async () => { await expect(hook.result.current.rotateCredentials('incorrect', 'admin', 'new-password-long')).rejects.toMatchObject({ status: 403, code: 'CURRENT_PASSWORD_INCORRECT' }); });
    expect(hook.result.current.state.phase).toBe('ready');
    expect(hook.result.current.api.currentBinding).toBe('binding-1');
    revoked = true;
    await act(async () => { await hook.result.current.rotateCredentials('current-password', 'admin', 'new-password-long').catch(() => {}); });
    await waitFor(() => expect(hook.result.current.state.phase).toBe('login'));
    expect(hook.result.current.state.credentials).toBeUndefined();
    hook.unmount();
  });

  it('never lets a delayed old rotation response replace the newer session announced by another tab', async () => {
    let broadcast!: { onmessage: ((event: { data: string }) => void) | null };
    vi.stubGlobal('BroadcastChannel', class {
      onmessage = null;
      constructor() { broadcast = this; }
      postMessage() {}
      close() {}
    });
    let finish!: (response: Response) => void;
    const response = new Promise<Response>((resolve) => { finish = resolve; });
    let active = session;
    vi.stubGlobal('fetch', vi.fn(async (url: string) => url === '/api/session' ? jsonResponse(active) : response));
    const hook = renderHook(() => useSession(), { wrapper: SessionProvider });
    await waitFor(() => expect(hook.result.current.state.phase).toBe('ready'));
    let rotation!: Promise<unknown>;
    act(() => { rotation = hook.result.current.rotateCredentials('current-password', 'admin', 'new-password-long').catch((error: unknown) => error); });
    active = { ...session, session: { ...session.session, binding: 'later-login' } };
    act(() => { broadcast.onmessage?.({ data: 'changed' }); });
    await waitFor(() => expect(hook.result.current.api.currentBinding).toBe('later-login'));
    await act(async () => { finish(jsonResponse({ reauthentication_required: true })); expect(await rotation).toMatchObject({ name: 'StaleRequest' }); });
    expect(hook.result.current.api.currentBinding).toBe('later-login');
    expect(hook.result.current.state.phase).toBe('ready');
    expect(hook.result.current.state.credentials).toBeUndefined();
    hook.unmount();
  });

  it('does not mask or interrupt a pending credential mutation when logout is clicked after closing its form', async () => {
    let finish!: (response: Response) => void;
    const response = new Promise<Response>((resolve) => { finish = resolve; });
    const fetcher = vi.fn(async (url: string) => url === '/api/session' ? jsonResponse(session) : response);
    vi.stubGlobal('fetch', fetcher);
    const hook = renderHook(() => useSession(), { wrapper: SessionProvider });
    await waitFor(() => expect(hook.result.current.state.phase).toBe('ready'));
    let rotation!: Promise<void>;
    act(() => { rotation = hook.result.current.rotateCredentials('current-password', 'admin', 'new-password-long'); });
    await act(async () => { await expect(hook.result.current.logout()).rejects.toMatchObject({ code: 'BUSY' }); });
    expect(hook.result.current.api.currentBinding).toBe('binding-1');
    expect(hook.result.current.state.phase).toBe('ready');
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/logout')).toHaveLength(0);
    await act(async () => { finish(jsonResponse({ reauthentication_required: true })); await rotation; });
    expect(hook.result.current.state).toMatchObject({ phase: 'login', credentials: 'changed' });
    hook.unmount();
  });

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
