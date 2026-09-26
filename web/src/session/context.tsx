import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ApiClient, ApiError, CookieUnavailable, StaleRequest, type SessionView, type TransportChange, type TransportView } from './client';
import { reloadConsole } from '../features/updates/reload';

export type SessionPhase = 'checking' | 'setup' | 'setup-unknown' | 'login' | 'ready' | 'connection-error' | 'cookie-unavailable' | 'logout-unknown' | 'transport-change';
export interface SessionState { phase: SessionPhase; expiresInSeconds?: number; error?: string; transport?: TransportView; nextOrigin?: string | null }

interface SessionContextValue {
  state: SessionState;
  api: ApiClient;
  login(username: string, password: string): Promise<void>;
  setup(username: string, password: string, toml: string, token: string): Promise<void>;
  logout(): Promise<void>;
  retryLogout(): Promise<void>;
  recheck(): Promise<void>;
  transportChanged(change: TransportChange): void;
  expectUpdateRestart(expected: boolean): void;
}

const SessionContext = createContext<SessionContextValue | null>(null);
const CHANNEL_NAME = 'parins-session-changed';

export function SessionProvider({ children }: { children: ReactNode }) {
  const api = useMemo(() => new ApiClient(), []);
  const [state, setState] = useState<SessionState>({ phase: 'checking' });
  const channel = useRef<BroadcastChannel | null>(null);
  const mounted = useRef(true);
  const bootId = useRef(0);
  const logoutBinding = useRef<string | null>(null);
  const logoutIntent = useRef(false);
  const authInFlight = useRef(false);
  const updateRestart = useRef(false);
  const expectUpdateRestart = useCallback((expected: boolean) => { updateRestart.current = expected; }, []);
  const phase = useRef<SessionPhase>(state.phase);
  phase.current = state.phase;

  const applySession = useCallback((view: SessionView) => {
    if (!mounted.current) return;
    if (!view.authenticated && updateRestart.current && !logoutIntent.current) {
      updateRestart.current = false;
      api.replaceBinding(null);
      setState({ phase: 'checking' });
      reloadConsole();
      return;
    }
    const binding = view.authenticated ? view.session!.binding : null;
    if (binding !== api.currentBinding) api.replaceBinding(binding);
    setState(view.setup_required ? { phase: 'setup', transport: view.transport } : view.authenticated
      ? { phase: 'ready', expiresInSeconds: view.session!.expires_in_seconds, transport: view.transport }
      : { phase: 'login', transport: view.transport });
  }, [api]);

  const recheck = useCallback(async () => {
    if (logoutIntent.current) return;
    const id = ++bootId.current;
    try {
      const view = await api.session();
      if (id !== bootId.current || !mounted.current || logoutIntent.current) return;
      applySession(view);
    } catch (error) {
      if (error instanceof StaleRequest || id !== bootId.current || !mounted.current || logoutIntent.current) return;
      setState((current) => current.phase === 'setup-unknown'
        ? { ...current, error: error instanceof Error ? error.message : undefined }
        : { phase: 'connection-error', error: error instanceof Error ? error.message : undefined });
    }
  }, [api, applySession]);

  const maskAndRecheck = useCallback(() => {
    if (logoutIntent.current) return;
    api.replaceBinding(null);
    setState({ phase: 'checking' });
    void recheck();
  }, [api, recheck]);

  useEffect(() => {
    mounted.current = true;
    api.setAuthEventHandler((event) => {
      if (event === 'unauthorized' && updateRestart.current && !logoutIntent.current) {
        updateRestart.current = false;
        api.replaceBinding(null);
        setState({ phase: 'checking' });
        reloadConsole();
        return;
      }
      if (event === 'transport-changed') {
        bootId.current += 1;
        api.replaceBinding(null);
        setState({ phase: 'transport-change', nextOrigin: null });
      } else maskAndRecheck();
    });
    if (typeof BroadcastChannel !== 'undefined') {
      const next = new BroadcastChannel(CHANNEL_NAME);
      next.onmessage = (event) => { if (event.data === 'changed') maskAndRecheck(); };
      channel.current = next;
    }
    const visible = () => { if (!document.hidden && phase.current === 'ready') void recheck(); };
    document.addEventListener('visibilitychange', visible);
    void recheck();
    return () => {
      mounted.current = false;
      bootId.current += 1;
      api.setAuthEventHandler(() => {});
      api.replaceBinding(null);
      document.removeEventListener('visibilitychange', visible);
      channel.current?.close();
      channel.current = null;
    };
  }, [api, maskAndRecheck, recheck]);

  const announce = () => channel.current?.postMessage('changed');

  // Only this tab serializes its own buttons. The server binding/realm checks own cross-tab safety.
  const authAction = useCallback(async <T,>(action: () => Promise<T>): Promise<T> => {
    if (authInFlight.current) throw new ApiError(409, 'BUSY', 'An authentication action is running');
    authInFlight.current = true;
    try { return await action(); }
    finally { authInFlight.current = false; }
  }, []);

  const transportChanged = useCallback((change: TransportChange) => {
    bootId.current += 1;
    logoutIntent.current = false;
    api.replaceBinding(null);
    setState({ phase: 'transport-change', nextOrigin: change.next_origin });
    announce();
    if (change.next_origin) window.setTimeout(() => {
      window.location.assign(`${change.next_origin}${window.location.pathname}${window.location.search}${window.location.hash}`);
    }, 0);
  }, [api]);

  const confirmLogin = useCallback(async () => {
    const view = await api.session();
    if (!view.authenticated || !view.session) throw new CookieUnavailable();
    applySession(view);
    announce();
  }, [api, applySession]);

  const login = useCallback(async (username: string, password: string) => {
    try {
      await authAction(async () => {
        const view = await api.session();
        if (view.authenticated) { applySession(view); return; }
        await api.request('login', 'POST', { username, password }, undefined, { unauthenticated: true });
        await confirmLogin();
      });
    } catch (error) {
      if (error instanceof CookieUnavailable) setState({ phase: 'cookie-unavailable' });
      throw error;
    }
  }, [api, applySession, authAction, confirmLogin]);

  const setup = useCallback(async (username: string, password: string, toml: string, token: string) => {
    let sent = false;
    const candidate = { change: null as TransportChange | null };
    try {
      await authAction(async () => {
        const view = await api.session();
        if (view.authenticated) { applySession(view); return; }
        if (!view.setup_required) { applySession(view); throw new ApiError(409, 'SETUP_DONE', 'Setup already completed'); }
        const preview = await api.request<{ valid: true; transport_change: TransportChange | null }>('setup/preview', 'POST', { toml },
          { 'X-PariNS-Setup': token }, { unauthenticated: true });
        candidate.change = preview.transport_change;
        sent = true;
        const result = await api.request<SessionView & { transport_change: TransportChange | null }>('setup', 'POST', { username, password, toml }, { 'X-PariNS-Setup': token }, { unauthenticated: true });
        sent = false;
        if (result.transport_change) { transportChanged(result.transport_change); return; }
        await confirmLogin();
      });
    } catch (error) {
      if (error instanceof CookieUnavailable) setState({ phase: 'cookie-unavailable' });
      if (sent && (error instanceof StaleRequest || error instanceof ApiError && ['NETWORK', 'BAD_RESPONSE'].includes(error.code))) {
        api.replaceBinding(null);
        setState({ phase: 'setup-unknown', nextOrigin: candidate.change?.next_origin ?? null });
      }
      throw error;
    }
  }, [api, applySession, authAction, confirmLogin, transportChanged]);

  const performLogout = useCallback(async (expected: string | null) => {
    try {
      await authAction(async () => {
        const view = await api.session();
        const actual = view.session?.binding ?? null;
        if (actual && actual !== expected) {
          // Never apply an old page's logout intention to a newer login.
          logoutIntent.current = false;
          applySession(view);
          throw new ApiError(409, 'SESSION_CHANGED', 'Another session is active');
        }
        if (actual && api.currentBinding !== actual) api.replaceBinding(actual);
        await api.request('logout', 'POST', {}, undefined, { unauthenticated: !actual });
        api.replaceBinding(null);
        logoutBinding.current = null;
        logoutIntent.current = false;
        setState({ phase: 'login' });
        announce();
      });
    } catch (error) {
      if (!(error instanceof ApiError && error.code === 'SESSION_CHANGED')) setState({ phase: 'logout-unknown' });
      throw error;
    }
  }, [api, applySession, authAction]);

  const logout = useCallback(async () => {
    updateRestart.current = false;
    logoutBinding.current = api.currentBinding;
    logoutIntent.current = true;
    bootId.current += 1;
    api.replaceBinding(null);
    setState({ phase: 'logout-unknown' }); // Mask private UI before touching the network.
    await performLogout(logoutBinding.current);
  }, [api, performLogout]);

  const retryLogout = useCallback(async () => {
    if (state.phase !== 'logout-unknown') return;
    await performLogout(logoutBinding.current);
  }, [performLogout, state.phase]);

  const value = useMemo<SessionContextValue>(() => ({ state, api, login, setup, logout, retryLogout, recheck, transportChanged, expectUpdateRestart }), [state, api, login, setup, logout, retryLogout, recheck, transportChanged, expectUpdateRestart]);
  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

export function useSession() {
  const value = useContext(SessionContext);
  if (!value) throw new Error('SessionProvider is required');
  return value;
}
