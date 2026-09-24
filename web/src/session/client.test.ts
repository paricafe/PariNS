import { describe, expect, it, vi } from 'vitest';
import { ApiClient, ApiError, StaleRequest } from './client';

const json = (status: number, value: unknown) => new Response(JSON.stringify(value), { status, headers: { 'content-type': 'application/json' } });
const view = { setup_required: false, authenticated: true, session: { binding: 'binding-1', expires_in_seconds: 3600 },
  transport: { scheme: 'http', origin: null, certificate_source: null } };

describe('management request ownership', () => {
  it('encodes statistics query values without relaxing API path validation', async () => {
    const fetcher = vi.fn(async () => json(200, {}));
    const api = new ApiClient(fetcher as typeof fetch);
    api.replaceBinding('binding-1');
    await api.request('stats', 'GET', undefined, undefined, { query: { range: 'custom', from_ms: '1&range=7d', to_ms: '20' } });
    expect(fetcher).toHaveBeenCalledWith('/api/stats?range=custom&from_ms=1%26range%3D7d&to_ms=20', expect.any(Object));
    await expect(api.request('stats?range=7d')).rejects.toThrow('Invalid API path');
    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  it('rejects the removed standalone DoH3 certificate source', async () => {
    const api = new ApiClient(vi.fn(async () => json(200, { ...view, transport: { ...view.transport, certificate_source: 'doh3' } })) as typeof fetch);
    await expect(api.session()).rejects.toMatchObject({ code: 'BAD_RESPONSE' });
  });

  it('calls the browser fetch with its global receiver by default', async () => {
    const fetcher = vi.fn(function (this: unknown) {
      expect(this).toBe(globalThis);
      return Promise.resolve(json(200, view));
    });
    vi.stubGlobal('fetch', fetcher);
    try { expect(await new ApiClient().session()).toEqual(view); }
    finally { vi.unstubAllGlobals(); }
  });

  it('restores a view with Cookie transport and no Bearer header', async () => {
    const fetcher = vi.fn(async () => json(200, view));
    const api = new ApiClient(fetcher as typeof fetch);
    expect(await api.session()).toEqual(view);
    const [, options] = fetcher.mock.calls[0] as unknown as [string, RequestInit];
    expect(options.credentials).toBe('same-origin');
    expect(options.cache).toBe('no-store');
    expect(options.redirect).toBe('error');
    expect((options.headers as Record<string, string>).Authorization).toBeUndefined();
  });

  it('requires binding before protected network traffic and sends it on requests', async () => {
    const fetcher = vi.fn(async () => json(200, { revision: 2 }));
    const api = new ApiClient(fetcher as typeof fetch);
    await expect(api.request('config')).rejects.toMatchObject({ code: 'UNAUTHORIZED' });
    expect(fetcher).not.toHaveBeenCalled();
    api.replaceBinding('binding-1');
    await api.request('config/validate', 'POST', { toml: 'test' });
    const [, options] = fetcher.mock.calls[0] as unknown as [string, RequestInit];
    expect(options.headers).toMatchObject({ 'X-PariNS-Session': 'binding-1', 'Content-Type': 'application/json' });
    expect(options.body).toBe(JSON.stringify({ toml: 'test' }));
  });

  it('invalidates in-flight response data when binding changes before JSON resolves', async () => {
    let resolveJson!: (value: unknown) => void;
    const response = { ok: true, status: 200, json: () => new Promise((resolve) => { resolveJson = resolve; }) } as Response;
    const api = new ApiClient(vi.fn(async () => response) as typeof fetch);
    api.replaceBinding('binding-1');
    const pending = api.request('status');
    await Promise.resolve();
    api.replaceBinding('binding-2');
    resolveJson({ revision: 1 });
    await expect(pending).rejects.toBeInstanceOf(StaleRequest);
  });

  it('masks 401 and rebases SESSION_CHANGED without replaying the request', async () => {
    const fetcher = vi.fn()
      .mockResolvedValueOnce(json(401, { error: { code: 'UNAUTHORIZED', message: 'expired' } }))
      .mockResolvedValueOnce(json(409, { error: { code: 'SESSION_CHANGED', message: 'replaced' } }));
    const api = new ApiClient(fetcher as typeof fetch);
    const events: string[] = [];
    api.setAuthEventHandler((event) => events.push(event));
    api.replaceBinding('binding-1');
    await expect(api.request('status')).rejects.toBeInstanceOf(ApiError);
    await expect(api.request('config', 'PUT', { toml: '', revision: 1 })).rejects.toBeInstanceOf(ApiError);
    expect(events).toEqual(['unauthorized', 'session-changed']);
    expect(fetcher).toHaveBeenCalledTimes(2);
  });

  it('reports a changed transport realm without replaying a write', async () => {
    const fetcher = vi.fn(async () => json(409, { error: { code: 'TRANSPORT_CHANGED', message: 'use new origin' } }));
    const api = new ApiClient(fetcher as typeof fetch);
    const events: string[] = [];
    api.setAuthEventHandler((event) => events.push(event));
    api.replaceBinding('old-binding');
    await expect(api.request('config', 'PUT', { toml: '', revision: 1 })).rejects.toMatchObject({ code: 'TRANSPORT_CHANGED' });
    expect(events).toEqual(['transport-changed']);
    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  it('rejects malformed session payloads instead of fabricating login', async () => {
    const api = new ApiClient(vi.fn(async () => json(200, { authenticated: true, setup_required: false, token: 'bad' })) as typeof fetch);
    await expect(api.session()).rejects.toMatchObject({ code: 'BAD_RESPONSE' });
  });

  it('requires a transport view when restoring the session', async () => {
    const api = new ApiClient(vi.fn(async () => json(200, { ...view, transport: undefined })) as typeof fetch);
    await expect(api.session()).rejects.toMatchObject({ code: 'BAD_RESPONSE' });
  });
});
