import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ApiClient, ApiError, CookieUnavailable, StaleRequest, WebLocksUnavailable, withAuthLock, type SessionView } from './client';

export type SessionPhase = 'checking' | 'setup' | 'login' | 'ready' | 'connection-error' | 'cookie-unavailable' | 'logout-unknown' | 'unsupported';
export interface SessionState { phase: SessionPhase; expiresInSeconds?: number; error?: string }

interface SessionContextValue {
  state: SessionState;
  api: ApiClient;
  login(username: string, password: string): Promise<void>;
  setup(username: string, password: string, toml: string, token: string): Promise<void>;
  logout(): Promise<void>;
  retryLogout(): Promise<void>;
  recheck(): Promise<void>;
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
  const phase = useRef<SessionPhase>(state.phase);
  phase.current = state.phase;

  const applySession = useCallback((view: SessionView) => {
    if (!mounted.current) return;
    const binding = view.authenticated ? view.session!.binding : null;
    if (binding !== api.currentBinding) api.replaceBinding(binding);
    setState(view.setup_required ? { phase: 'setup' } : view.authenticated
      ? { phase: 'ready', expiresInSeconds: view.session!.expires_in_seconds }
      : { phase: 'login' });
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
      setState({ phase: 'connection-error', error: error instanceof Error ? error.message : undefined });
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
    api.setAuthEventHandler(() => maskAndRecheck());
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

  const confirmLogin = useCallback(async () => {
    const view = await api.session();
    if (!view.authenticated || !view.session) throw new CookieUnavailable();
    applySession(view);
    announce();
  }, [api, applySession]);

  const login = useCallback(async (username: string, password: string) => {
    try {
      await withAuthLock(async () => {
        const view = await api.session();
        if (view.authenticated) { applySession(view); return; }
        await api.request('login', 'POST', { username, password }, undefined, { unauthenticated: true });
        await confirmLogin();
      });
    } catch (error) {
      if (error instanceof CookieUnavailable) setState({ phase: 'cookie-unavailable' });
      if (error instanceof WebLocksUnavailable) setState({ phase: 'unsupported' });
      throw error;
    }
  }, [api, applySession, confirmLogin]);

  const setup = useCallback(async (username: string, password: string, toml: string, token: string) => {
    try {
      await withAuthLock(async () => {
        const view = await api.session();
        if (view.authenticated) { applySession(view); return; }
        if (!view.setup_required) { applySession(view); throw new ApiError(409, 'SETUP_DONE', 'Setup already completed'); }
        await api.request('setup', 'POST', { username, password, toml }, { 'X-PariNS-Setup': token }, { unauthenticated: true });
        await confirmLogin();
      });
    } catch (error) {
      if (error instanceof CookieUnavailable) setState({ phase: 'cookie-unavailable' });
      if (error instanceof WebLocksUnavailable) setState({ phase: 'unsupported' });
      throw error;
    }
  }, [api, applySession, confirmLogin]);

  const performLogout = useCallback(async (expected: string | null) => {
    try {
      await withAuthLock(async () => {
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
      if (error instanceof WebLocksUnavailable) setState({ phase: 'unsupported' });
      else if (!(error instanceof ApiError && error.code === 'SESSION_CHANGED')) setState({ phase: 'logout-unknown' });
      throw error;
    }
  }, [api, applySession]);

  const logout = useCallback(async () => {
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

  const value = useMemo<SessionContextValue>(() => ({ state, api, login, setup, logout, retryLogout, recheck }), [state, api, login, setup, logout, retryLogout, recheck]);
  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

export function useSession() {
  const value = useContext(SessionContext);
  if (!value) throw new Error('SessionProvider is required');
  return value;
}
