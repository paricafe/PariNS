export interface SessionView {
  setup_required: boolean;
  authenticated: boolean;
  session: { binding: string; expires_in_seconds: number } | null;
  transport: TransportView;
}

export interface TransportView {
  scheme: 'http' | 'https';
  origin: string | null;
  certificate_source: 'doh' | 'doh3' | null;
}

export interface TransportChange {
  from: 'http' | 'https';
  to: 'http' | 'https';
  next_origin: string | null;
  reauthenticate: boolean;
  requires_http_confirmation: boolean;
}

export class ApiError extends Error {
  constructor(readonly status: number, readonly code: string, message: string) {
    super(message);
    this.name = 'ApiError';
  }
}

export class StaleRequest extends Error {
  constructor() { super('Request belongs to an earlier session'); this.name = 'StaleRequest'; }
}

export class CookieUnavailable extends Error {
  constructor() { super('The browser did not establish a login session'); this.name = 'CookieUnavailable'; }
}

export type ApiMethod = 'GET' | 'POST' | 'PUT';
type AuthEvent = 'unauthorized' | 'session-changed' | 'transport-changed';

function hasSession(value: unknown): value is SessionView {
  if (!value || typeof value !== 'object') return false;
  const view = value as Record<string, unknown>;
  if (typeof view.setup_required !== 'boolean' || typeof view.authenticated !== 'boolean') return false;
  const transport = view.transport as Record<string, unknown> | undefined;
  if (!transport || !['http', 'https'].includes(String(transport.scheme)) ||
    !(transport.origin === null || typeof transport.origin === 'string') ||
    ![null, 'doh', 'doh3'].includes(transport.certificate_source as null | string)) return false;
  if (view.session === null) return !view.authenticated;
  const session = view.session as Record<string, unknown> | undefined;
  return Boolean(view.authenticated && session && typeof session.binding === 'string' && session.binding.length > 0 && typeof session.expires_in_seconds === 'number');
}

// Only this layer knows the session binding. It is never persisted or broadcast.
export class ApiClient {
  private epoch = 0;
  private binding: string | null = null;
  private pending = new Set<AbortController>();
  private onAuthEvent: ((event: AuthEvent) => void) | null = null;

  constructor(private readonly fetcher: typeof fetch = (input, init) => globalThis.fetch(input, init)) {}

  setAuthEventHandler(handler: (event: AuthEvent) => void) { this.onAuthEvent = handler; }
  get currentBinding() { return this.binding; }
  get currentEpoch() { return this.epoch; }

  replaceBinding(binding: string | null) {
    this.epoch += 1;
    this.binding = binding;
    for (const controller of this.pending) controller.abort();
    this.pending.clear();
  }

  async request<T = unknown>(path: string, method: ApiMethod = 'GET', body?: unknown, extraHeaders?: Record<string, string>, options?: { unauthenticated?: boolean }): Promise<T> {
    if (!/^[a-z0-9/-]+$/i.test(path) || path.startsWith('/')) throw new Error('Invalid API path');
    const owner = this.epoch;
    const binding = this.binding;
    const headers: Record<string, string> = { ...extraHeaders };
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    if (!options?.unauthenticated && path !== 'session' && path !== 'template' && path !== 'login' && path !== 'setup') {
      if (!binding) throw new ApiError(401, 'UNAUTHORIZED', 'Sign in required');
      headers['X-PariNS-Session'] = binding;
    }
    const controller = new AbortController();
    this.pending.add(controller);
    try {
      const response = await this.fetcher(`/api/${path}`, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        credentials: 'same-origin',
        cache: 'no-store',
        redirect: 'error',
        signal: controller.signal,
      });
      this.ensureCurrent(owner);
      let data: unknown;
      try { data = await response.json(); }
      catch { throw new ApiError(response.status, 'BAD_RESPONSE', 'Invalid server response'); }
      this.ensureCurrent(owner);
      if (!response.ok) {
        const detail = data && typeof data === 'object' && 'error' in data && data.error && typeof data.error === 'object'
          ? data.error as { code?: unknown; message?: unknown } : {};
        const code = typeof detail.code === 'string' ? detail.code : 'HTTP';
        if (response.status === 401 && binding) this.onAuthEvent?.('unauthorized');
        if (response.status === 409 && code === 'SESSION_CHANGED') this.onAuthEvent?.('session-changed');
        if (response.status === 409 && code === 'TRANSPORT_CHANGED') this.onAuthEvent?.('transport-changed');
        throw new ApiError(response.status, code, typeof detail.message === 'string' ? detail.message : `HTTP ${response.status}`);
      }
      return data as T;
    } catch (error) {
      this.ensureCurrent(owner);
      if (error instanceof ApiError) throw error;
      throw new ApiError(0, 'NETWORK', error instanceof Error ? error.message : 'Network unavailable');
    } finally {
      this.pending.delete(controller);
    }
  }

  async session(): Promise<SessionView> {
    const view = await this.request<unknown>('session', 'GET', undefined, undefined, { unauthenticated: true });
    if (!hasSession(view)) throw new ApiError(0, 'BAD_RESPONSE', 'Invalid session response');
    return view;
  }

  private ensureCurrent(epoch: number) { if (epoch !== this.epoch) throw new StaleRequest(); }
}
